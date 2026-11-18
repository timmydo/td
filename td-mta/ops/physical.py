"""Physical recovery point and isolated rehearsal for the dedicated claw1 primary."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
import threading
import time
import uuid

MAIL = Path('/var/lib/td-mta/mail')
PGDATA = MAIL/'postgres'
TOOLS = Path('/opt/td-mta/postgresql/usr/lib/postgresql/17/bin')
CONTROL = Path('/var/lib/td-mta-operator/physical-backup')
VOLUME = Path('/data/td-mta-physical-recovery')
SYSTEM = '7698635942461075926'
GIB = 1024**3


def require(value, message):
    if not value:
        raise RuntimeError(message)


def digest(path):
    with path.open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()


def durable(path, value):
    tmp = path.with_suffix('.pending')
    with tmp.open('w') as out:
        json.dump(value, out, indent=2)
        out.write('\n')
        out.flush()
        os.fsync(out.fileno())
    os.replace(tmp, path)
    fd = os.open(path.parent, os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


class Recovery:
    def __init__(self, host):
        self.host = host
        require(VOLUME.is_dir() and VOLUME.stat().st_dev != PGDATA.stat().st_dev
                and VOLUME.stat().st_mode & 0o777 == 0o700, 'private recovery mount')
        self.work = Path(tempfile.mkdtemp(prefix='deploy-', dir=VOLUME))
        self.runtime = Path(tempfile.mkdtemp(prefix='td-mta-recovery-', dir='/run'))
        self.runtime.chmod(0o711)
        self.wal = self.work/'wal'
        self.archive = self.work/'archive'
        for path in (self.wal, self.archive, self.runtime/'socket'):
            path.mkdir(mode=0o700)
        for path in (self.archive, self.runtime/'socket'):
            shutil.chown(path, 'td-mta', 'td-mta')
        self.point = 'td_cut_'+uuid.uuid4().hex
        self.receipt = dict(point=self.point, status='preparing')
        self.receiver = None
        self.unit = None
        self.stop_guard = threading.Event()
        self.guard_failure = None
        self.supervisor = None
        self.started = time.monotonic()

    def source(self):
        values = self.host.sql('replication','IDENTIFY_SYSTEM').split('|')
        require(len(values)==4 and values[0]==SYSTEM and values[1].isdigit(), 'primary identity')
        return values[:2]

    def guard(self):
        while not self.stop_guard.wait(.5):
            try:
                require(time.monotonic()-self.started < 1800, 'backup preparation timeout')
                require(shutil.disk_usage(self.work).free > 8*GIB, 'recovery disk reserve')
                size = sum(f.stat().st_size for f in self.wal.iterdir() if f.is_file())
                require(size <= GIB, 'WAL spool budget')
                require(self.receiver.poll() is None, 'WAL receiver disconnected')
            except FileNotFoundError:  # receivewal renames a completed segment.
                continue
            except Exception as error:
                self.guard_failure = str(error)
                if self.receiver.poll() is None:
                    self.receiver.send_signal(signal.SIGINT)
                return

    def wait(self, predicate, seconds, what):
        end = time.monotonic()+seconds
        while time.monotonic() < end:
            self.host.remaining()
            require(self.guard_failure is None, self.guard_failure)
            if predicate():
                return
            time.sleep(.15)
        raise RuntimeError(what+' timed out')

    def streaming(self):
        pid = self.host.sql('owner', "SELECT pid FROM pg_stat_replication WHERE application_name='td_cutover_wal' AND usename='td_mta_cutover_backup'")
        if not pid.isdecimal():
            return False
        title = Path('/proc',pid,'cmdline').read_bytes()
        return b'walsender' in title and b'streaming' in title

    def prepare(self):
        h = self.host
        self.identity = self.source()
        h.check_source()
        settings = h.sql('owner', """SELECT current_setting('wal_level') IN ('replica','logical'),
            current_setting('full_page_writes'),current_setting('synchronous_standby_names')='',
            current_setting('max_wal_senders')::int-(SELECT count(*) FROM pg_stat_replication)>=3,
            NOT EXISTS (SELECT FROM pg_tablespace WHERE spcname NOT IN ('pg_default','pg_global')),
            NOT EXISTS (SELECT FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
                        WHERE n.nspname='td_mta' AND c.relpersistence<>'p'),
            NOT EXISTS (SELECT FROM pg_database WHERE NOT datistemplate AND datname NOT IN ('td_mta','postgres'))""")
        require(settings=='t|on|t|t|t|t|t', 'dedicated physical backup prerequisites')
        base = self.work/'base'
        size = int(h.sql('owner', 'SELECT sum(pg_database_size(oid)) FROM pg_database'))
        require(shutil.disk_usage(self.work).free > 4*size + 9*GIB, 'backup capacity')
        with (self.work/'receiver.log').open('w') as log:
            self.receiver = subprocess.Popen([TOOLS/'pg_receivewal','-wn','--verbose','--synchronous',
                '-D',self.wal,'-d','service=cutover_repl application_name=td_cutover_wal'],
                env=h.pg_env(CONTROL/'service.conf'), stdout=log, stderr=log)
        self.supervisor = threading.Thread(target=self.guard, daemon=True)
        self.supervisor.start()
        self.wait(self.streaming, 20, 'streaming replication sender')
        h.run([TOOLS/'pg_basebackup','-w','--no-slot','--wal-method=stream','-D',base,
               '-d','service=cutover_repl'], env=h.pg_env(CONTROL/'service.conf'), timeout=1200)
        h.run([TOOLS/'pg_verifybackup',base], timeout=300)
        manifest = json.loads((base/'backup_manifest').read_bytes())
        ranges = manifest['WAL-Ranges']
        require(str(manifest['System-Identifier'])==SYSTEM and len(ranges)==1
                and str(ranges[0]['Timeline'])==self.identity[1], 'base identity and timeline')
        self.receipt.update(system_identifier=SYSTEM, timeline=self.identity[1],
                            manifest_sha256=digest(base/'backup_manifest'), wal_range=ranges[0])
        # External cluster configuration, credentials, keys, installed libraries and release.
        paths = [MAIL/'postgresql.conf', MAIL/'tls', MAIL/'owner-connection',MAIL/'owner-password',
                 MAIL/'runtime-connection',MAIL/'runtime-password',MAIL/'bootstrap.conf',
                 *MAIL.glob('*.toml'),MAIL/'config-version',MAIL/'account-id',
                 Path('/var/lib/td-mta/backup-control'),Path('/opt/td-mta/postgresql'),
                 h.previous.parent, CONTROL/'service.conf',CONTROL/'marker.pass',CONTROL/'repl.pass']
        h.run(['tar','-cf',self.work/'bootstrap.tar','-C','/',
               *[p.relative_to('/') for p in paths]], timeout=180)
        self.receipt['bootstrap_sha256'] = digest(self.work/'bootstrap.tar')
        # Rehearse on an independent writable cluster in its own loopback network.
        self.start_copy('migration', standby=False)
        pid = h.out(['systemctl','show',self.unit,'-p','MainPID','--value'])
        require(pid.isdecimal() and int(pid)>0 and os.stat('/proc/'+pid+'/ns/net').st_ino
                != os.stat('/proc/self/ns/net').st_ino, 'trial network isolation')
        ns = ['nsenter','--target',pid,'--net','--']
        self.wait(lambda: h.ready_db(ns), 40, 'migration rehearsal startup')
        require(h.sql('owner','SELECT pg_is_in_recovery()', ns=ns)=='f','writable rehearsal')
        h.upgrade(ns)
        self.stop_copy()
        self.start_copy('replay', standby=True)
        self.wait(lambda: self.local_ready(), 45, 'archive recovery startup')
        require(self.local("SELECT pg_is_in_recovery(),system_identifier,timeline_id FROM pg_control_system(),pg_control_checkpoint()")
                =='t|'+SYSTEM+'|'+self.identity[1], 'archive recovery identity')
        lsn, segment = self.marker('td_probe_'+uuid.uuid4().hex)
        self.feed(segment)
        self.wait(lambda: self.local("SELECT pg_is_in_recovery() AND pg_last_wal_replay_lsn()>='"+lsn+"'::pg_lsn")=='t', 60, 'intermediate replay')
        self.receipt['intermediate_lsn'] = lsn
        durable(self.work/'receipt.json', self.receipt)

    def start_copy(self, name, standby):
        h = self.host
        data = self.work/name
        h.run(['cp','-a','--reflink=auto',self.work/'base',data], timeout=180)
        require(not any(x.is_symlink() for x in data.rglob('*')), 'external trial pathname')
        for signal_file in ('recovery.signal','standby.signal'):
            (data/signal_file).unlink(missing_ok=True)
        (data/'postgresql.auto.conf').write_bytes(b'')
        if standby:
            (data/'standby.signal').touch()
        h.run(['chown','-R','td-mta:td-mta',data])
        r = self.runtime
        mount = r/'cluster'
        mount.mkdir(exist_ok=True)
        (r/'wal').mkdir(exist_ok=True)
        for f in ('server.pem','server.key'):
            h.run(['install','-m','0600','-o','td-mta','-g','td-mta',MAIL/'tls'/f,r/f])
        hba = ('local all setup_admin peer map=trial\n' if standby else
               'hostssl td_mta td_mta_owner 127.0.0.1/32 scram-sha-256\n'
               'hostssl td_mta td_mta_runtime 127.0.0.1/32 scram-sha-256\n')
        (r/'hba').write_text(hba)
        (r/'ident').write_text('trial root setup_admin\n')
        config = dict(data_directory=mount,hba_file=r/'hba',ident_file=r/'ident',
                      listen_addresses='' if standby else '127.0.0.1',port='35432',
                      unix_socket_directories=r/'socket' if standby else '',
                      unix_socket_permissions='0700',ssl='off' if standby else 'on',
                      ssl_cert_file=r/'server.pem',ssl_key_file=r/'server.key',
                      shared_preload_libraries='',archive_mode='off',archive_command='',
                      primary_conninfo='',primary_slot_name='',hot_standby='on',
                      log_statement='none',log_min_error_statement='panic')
        for limit in ('max_connections','max_worker_processes','max_wal_senders',
                      'max_locks_per_transaction','max_prepared_transactions','track_commit_timestamp'):
            value=h.sql('owner', 'SHOW '+limit)
            require(value.isdecimal() or value in ('on','off'), 'unexpected PostgreSQL setting')
            config[limit]=value
        if standby:
            config.update(recovery_target_timeline='current',recovery_target_action='pause',
                          recovery_target_name=self.point,
                          restore_command=f'test -f {r}/wal/%f && cp -- {r}/wal/%f %p')
        (r/'conf').write_text(''.join(f"{k}='{v}'\n" for k,v in config.items()))
        for f in ('conf','hba','ident'):
            os.chmod(r/f,0o640)
            shutil.chown(r/f,'root','td-mta')
        self.unit='td-mta-trial-'+self.point+'-'+name
        props=['BindsTo=td-mta-'+h.work.name+'.service','After=td-mta-'+h.work.name+'.service','User=td-mta','Group=td-mta','PrivateNetwork=yes','PrivateTmp=yes',
               'PrivateIPC=yes','PrivateDevices=yes','NoNewPrivileges=yes','ProtectSystem=strict',
               'ProtectHome=yes','KillSignal=SIGINT','TimeoutStopSec=45','UMask=0077',
               'InaccessiblePaths=/var/lib/td-mta /var/lib/td-mta-operator /data',
               f'BindPaths={data}:{mount}',f'BindReadOnlyPaths={self.archive}:{r}/wal',
               f'ReadWritePaths={r}/socket {mount}']
        h.run(['systemd-run','--quiet','--collect','--unit='+self.unit,'--property=Type=exec',
               *['--property='+s for s in props],TOOLS/'postgres','-D',mount,
               '-c','config_file='+str(r/'conf')])

    def local_ready(self):
        return self.host.run([TOOLS/'pg_isready','-q','-h',self.runtime/'socket','-p','35432',
                             '-U','setup_admin','-d','td_mta'],check=False).returncode==0

    def local(self, query):
        return self.host.out([TOOLS/'psql','-XwqAt','-v','ON_ERROR_STOP=1','-h',self.runtime/'socket',
                              '-p','35432','-U','setup_admin','-d','td_mta','-c',query])

    def marker(self, name):
        require(self.source()==self.identity and self.streaming(), 'source or receiver changed')
        query=("SELECT pg_create_restore_point('"+name+"') AS lsn "+chr(92)+"gset\n"
               "SELECT :'lsn',pg_walfile_name(:'lsn');\nSELECT pg_switch_wal();\n")
        lines=self.host.sql('marker',query,script=True).splitlines()
        require(len(lines)==2,'restore point result lost')
        lsn,segment=lines[0].split('|')
        require(re.fullmatch('[A-F0-9]+/[A-F0-9]+',lsn) and re.fullmatch('[A-F0-9]{24}',segment),
                'invalid restore point result')
        return lsn,segment

    def feed(self, segment):
        self.wait(lambda:(self.wal/segment).is_file(),60,'completed WAL segment')
        for f in sorted(self.wal.iterdir()):
            dest=self.archive/f.name
            if re.fullmatch('[0-9A-F]{24}',f.name) and not dest.exists():
                tmp=dest.with_suffix('.tmp')
                self.host.run(['cp','--reflink=auto',f,tmp])
                shutil.chown(tmp,'td-mta','td-mta')
                tmp.replace(dest)

    def seal(self):
        lsn,segment=self.marker(self.point)
        self.receipt.update(lsn=lsn,segment=segment)
        durable(self.work/'receipt.json',self.receipt)
        self.feed(segment)
        condition=("SELECT pg_is_in_recovery() AND pg_get_wal_replay_pause_state()='paused' "
                   "AND pg_last_wal_replay_lsn()='"+lsn+"'::pg_lsn")
        self.wait(lambda:self.local(condition)=='t',60,'exact recovery pause')
        compare=(CONTROL/'compare-selected.sql').read_text()
        before=self.host.sql('owner',compare,script=True)
        require(before and self.local(compare)==before,'selected mail comparison')
        require(self.source()==self.identity and self.streaming(),'final source identity')
        self.stop_receiver()
        size=int(self.host.sql('owner',"SELECT pg_size_bytes(current_setting('wal_segment_size'))"))
        hi,lo=self.receipt['wal_range']['Start-LSN'].split('/')
        first=((int(hi,16)<<32)+int(lo,16))//size
        per=2**32//size
        last=int(segment[8:16],16)*per+int(segment[16:],16)
        require(0<=last-first<=GIB//size,'archive continuity range')
        for i in range(first,last+1):
            name=f'{int(self.identity[1]):08X}{i//per:08X}{i%per:08X}'
            require((self.wal/name).stat().st_size==size,'archive continuity')
        require(self.local(condition)=='t','trial lost pause')
        self.stop_copy()
        self.receipt.update(status='verified',selected=before,exact_lsn=True,
                            receiver_stopped=True,trial_stopped=True)
        durable(self.work/'receipt.json',self.receipt)
        self.host.run(['sync','-f',self.work])

    def stop_receiver(self):
        self.stop_guard.set()
        if self.supervisor:
            self.supervisor.join()
        if self.receiver is not None:
            proc,self.receiver=self.receiver,None
            if proc.poll() is None:
                proc.send_signal(signal.SIGINT)
            try:
                code=proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
                raise
            require(code==0 and self.guard_failure is None,'WAL receiver failed')

    def stop_copy(self):
        if self.unit is not None:
            self.host.run(['systemctl','stop',self.unit],timeout=50,check=False)
            state=self.host.out(['systemctl','show',self.unit,'-p','ActiveState','--value'])
            require(state in ('inactive','failed'), 'isolated PostgreSQL still running')
            self.unit=None

    def close(self):
        try:
            self.stop_receiver()
        finally:
            self.stop_copy()
            for name in ('migration','replay','archive'):
                if (self.work/name).is_dir():
                    shutil.rmtree(self.work/name)
            shutil.rmtree(self.runtime)
