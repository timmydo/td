"""Production deployment supervisor. Invoked by td-mta/deploy on claw1."""
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import signal
import smtplib
import socket
import ssl
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import zipfile

from physical import CONTROL, GIB, MAIL, PGDATA, SYSTEM, TOOLS, Recovery, digest, durable, require

OP = Path('/var/lib/td-mta-operator')
RELEASES = Path('/opt/td-mta/lib/td-mta/releases')
ACTIVE = Path('/opt/td-mta/bin/td-mta')
OWNER = '/var/lib/td-mta/backup-control/service.conf'
PROFILES = ('frontend','receive','relay','migadu')
UNITS = ['td-mta-'+x+'.service' for x in (*PROFILES,'acme-production')]
FENCE = OP/'deployment-fence'
PG = 'td-mta-postgres.service'
SAFE_PATH = '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
LIMITS = {'SELECT','SELECT, INSERT, DELETE','SELECT, INSERT, UPDATE, DELETE','SELECT, UPDATE (singleton)'}


class Probes:
    def __init__(self):
        self.stop = threading.Event()
        self.samples = {'smtp':[], 'jmap':[]}
        self.threads = []
        self.tls = ssl.create_default_context()

    def probe(self, name):
        domain='mx.timmydouglas.com'
        if name=='smtp':
            with smtplib.SMTP(timeout=1) as smtp:
                require(smtp.connect('127.0.0.1',25)[0]==220,'SMTP greeting')
                smtp._host=domain
                require(smtp.ehlo('probe.invalid')[0]==250,'SMTP EHLO')
                require(smtp.starttls(context=self.tls)[0]==220,'SMTP STARTTLS')
                require(smtp.ehlo('probe.invalid')[0]==250,'encrypted EHLO')
        else:
            with socket.create_connection(('127.0.0.1',443),timeout=1) as sock:
                with self.tls.wrap_socket(sock,server_hostname=domain) as conn:
                    conn.sendall(b'GET /.well-known/jmap HTTP/1.1\r\nHost: mx.timmydouglas.com\r\nConnection: close\r\n\r\n')
                    require(conn.makefile('rb').readline(1024).split()[1:2]==[b'401'],'JMAP challenge')

    def sample(self, name):
        while not self.stop.is_set():
            start=time.monotonic()
            try:
                self.probe(name)
                ok=True
            except (OSError,RuntimeError,smtplib.SMTPException):
                ok=False
            self.samples[name].append((time.time(),ok))
            self.stop.wait(max(0,.5-(time.monotonic()-start)))

    def start(self):
        for name in self.samples:
            self.probe(name)
            t=threading.Thread(target=self.sample,args=(name,),daemon=True)
            t.start()
            self.threads.append(t)

    def finish(self):
        self.stop.set()
        for t in self.threads:
            t.join()
        durable(Path.cwd()/'listener-samples.json',self.samples)


class Deployment:
    def __init__(self, job):
        require(socket.gethostname()=='claw1' and os.getuid()==0,'claw1 operator identity')
        self.work=Path.cwd()
        require(self.work.parent==OP and re.fullmatch(r'deploy-[A-Za-z0-9]{8}',self.work.name),'operator workspace')
        self.job=job
        for k in ('revision','previous','tree','main','controller'):
            require(re.fullmatch('[a-f0-9]{40}',job[k]),'invalid pinned revision')
        require(type(job['budget']) is int and 1<=job['budget']<=3600,'invalid window')
        self.previous=RELEASES/('git-'+job['previous'])/'td-mta'
        self.candidate=RELEASES/('git-'+job['revision'])/'td-mta'
        self.record=dict(job,status='preflight',started=time.time(),events=[])
        self.until=None
        self.fenced=False
        self.admitting=False
        self.pg_stopped=False
        self.firewall=False
        self.table="td_mta_"+self.work.name.replace("-","_")
        self.recovery=None
        self.cold=self.work/'candidate-pgdata'
        self.original=self.work/'original-pgdata'
        self.probes=Probes()
        self.drops=[]
        self.boot_permit=Path('/run/td-mta-deployment-primary.permit')
        self.timers=[]
        self.base_env=dict(PATH=SAFE_PATH,HOME='/root',LANG='C',LC_ALL='C')

    def event(self, what):
        self.record['events'].append([time.time(),what])
        self.record['status']=what
        durable(self.work/'receipt.json',self.record)
        print(what,flush=True)

    def remaining(self):
        if self.until is None:
            return float('inf')
        value=self.until-time.monotonic()
        require(value>0,'listener interruption budget exhausted')
        return value

    def pg_env(self, profile):
        return dict(self.base_env, PATH=str(TOOLS)+':'+SAFE_PATH,
                    LD_LIBRARY_PATH='/opt/td-mta/postgresql/usr/lib/x86_64-linux-gnu',
                    PGSERVICEFILE=str(profile),PGCONNECT_TIMEOUT='3')

    def run(self,args,*,env=None,timeout=40,check=True,**kw):
        # Command stderr can contain private paths or SQL context. Only report
        # the tool and exit code; build diagnostics use an explicit private file.
        timeout=min(timeout,self.remaining())
        try:
            result=subprocess.run(list(map(str,args)),env=env or self.pg_env(OWNER),
                timeout=timeout,stderr=kw.pop('stderr',subprocess.DEVNULL),
                stdout=kw.pop('stdout',subprocess.DEVNULL),**kw)
        except subprocess.TimeoutExpired:
            raise RuntimeError(str(args[0])+' timed out') from None
        if check and result.returncode:
            raise RuntimeError(f'{args[0]} exited {result.returncode}')
        return result

    def out(self,args,**kw):
        return self.run(args,stdout=subprocess.PIPE,text=True,**kw).stdout.strip()

    def sql(self,role,query,ns=(),script=False):
        profiles={'owner':'service=td-mta-backup','marker':'service=cutover_marker',
                  'replication':'service=cutover_repl dbname=replication replication=true'}
        conf=OWNER if role=='owner' else CONTROL/'service.conf'
        args=[*ns,TOOLS/'psql','-XwqAt','-v','ON_ERROR_STOP=1',profiles[role]]
        if script:
            return self.out(args,input=query+'\n',env=self.pg_env(conf))
        return self.out([*args,'-c',query],env=self.pg_env(conf))

    def ready_db(self,ns=()):
        return self.run([*ns,TOOLS/'psql','-XwqAt','service=td-mta-backup','-c','select 1'],
                        check=False,timeout=4).returncode==0

    def native(self,bin,op,*extra,ns=(),runtime=False):
        result=self.out([*ns,'runuser','-u','td-mta','--',bin,'postgres',op,'--connection-file',
                         MAIL/('runtime-connection' if runtime else 'owner-connection'),*extra])
        obj=json.loads(result)
        require(obj.get('status')=='ok','candidate '+op+' qualification')
        return obj

    def check_source(self):
        pid=self.out(['systemctl','show',PG,'-p','MainPID','--value'])
        require(pid.isdecimal() and int(pid)>0 and Path('/proc',pid,'cwd').resolve()==PGDATA,
                'unexpected primary data directory')
        metadata=self.out([TOOLS/'pg_controldata',PGDATA])
        require(any(line.split(':',1)[-1].strip()==SYSTEM for line in metadata.splitlines()
                    if line.startswith('Database system identifier:')) and
                self.sql('owner','SELECT pg_is_in_recovery()')=='f','production primary')

    def config(self,bin,ns=()):
        current=self.native(bin,'schema-inspect',ns=ns)
        require(current['store_epoch']==self.epoch,'store epoch changed')
        require(self.native(bin,'runtime-check',ns=ns,runtime=True)['store_epoch']==self.epoch,
                'runtime epoch changed')
        selected=self.sql('owner',"SELECT encode(version_id,'hex') FROM td_mta.config_selection",ns=ns)
        require(selected==self.selected,'selected configuration changed')
        self.native(bin,'config-check','--epoch',self.epoch,'--version',selected,ns=ns)

    def healthy(self,bin):
        for unit in [PG,*UNITS]:
            self.run(['systemctl','is-active','--quiet',unit])
            if unit!=PG:
                pid=self.out(['systemctl','show',unit,'-p','MainPID','--value'])
                require(pid.isdecimal() and Path('/proc',pid,'exe').resolve()==bin,'wrong running release')
        for role in PROFILES:
            self.wait(lambda:self.run(['runuser','-u','td-mta','--',bin,'status','--json','--require-ready',
                      '--runtime',MAIL/role/'runtime'],check=False).returncode==0,120,role+' readiness')
        pid=self.out(['systemctl','show',UNITS[-1],'-p','MainPID','--value'])
        require('pid='+pid+',' in self.out(['ss','-H','-ltnp','sport = :80']),'ACME listener')
        self.run(['runuser','-u','td-mta','--',bin,'acme-status','--connection-file',
                  MAIL/'runtime-connection','--certificate-profile','production','--require-ready',
                  '--minimum-validity-seconds','2592000'])
        require(self.out(['systemctl','show',UNITS[-1],'-p','MainPID','--value'])==pid,'ACME changed')
        self.run(['systemctl','start','td-mta-maint-backup-status.service','td-mta-health.service'])
        require(not self.out(['systemctl','list-units','--state=failed','--no-legend','td-mta-*']),'failed td-mta units')

    def build(self):
        if self.candidate.parent.exists():
            stamp=json.loads((self.candidate.parent/'deployment-inputs.json').read_text())
            require(stamp['revision']==self.job['revision'] and stamp['tree']==self.job['tree']
                    and stamp['binary_sha256']==digest(self.candidate),'retained release provenance')
            self.record['binary_sha256']=stamp['binary_sha256']
            return
        user='timmy'
        base=Path('/home/timmy')
        repo=base/'src/td'
        prefix=Path(self.out(['runuser','-u',user,'--','mktemp','-d',base/'.local/state/td-mta-candidates/deploy-XXXXXXXX']))
        source=prefix/'src'
        build_env=dict(self.base_env,HOME=str(base),PATH=str(base/'.cargo/bin')+':'+SAFE_PATH)
        def as_user(args,**kw):
            return self.run(['runuser','-u',user,'--',*args],env=build_env,**kw)
        rev=self.job['revision']
        as_user(['git','-C',repo,'worktree','add','--detach',source,rev])
        try:
            result=self.out(['runuser','-u',user,'--','git','-C',source,'rev-parse','HEAD^{tree}'])
            require(result==self.job['tree'],'remote checkout tree mismatch')
            with (self.work/'build.log').open('wb') as log:
                as_user([source/'install-td-mta','--prefix',prefix/'installed'],
                        stdout=log,stderr=subprocess.STDOUT,timeout=3600,cwd=source)
            exe=Path(self.out(['runuser','-u',user,'--','readlink','-e',prefix/'installed/bin/td-mta']))
            require(exe.is_relative_to(prefix/'installed/lib/td-mta/releases') and exe.name=='td-mta','build release path')
            expected=self.out(['runuser','-u',user,'--','sha256sum',exe]).split()[0]
            # Read the build tree as its owner. Extract only bounded regular
            # files and directories before granting the release its final name.
            with tempfile.TemporaryFile(dir=self.work) as tmp:
                as_user(['prlimit','--fsize=536870912','--','tar','-cf','-','-C',exe.parent,'.'],stdout=tmp,timeout=120)
                tmp.seek(0)
                stage=Path(tempfile.mkdtemp(prefix='.stage-',dir=RELEASES))
                try:
                    with tarfile.open(fileobj=tmp,mode='r:') as tar:
                        total=0
                        for member in tar:
                            parts=Path(member.name)
                            require(not parts.is_absolute() and '..' not in parts.parts and
                                    (member.isdir() or member.isfile()),'unsafe release member')
                            dest=stage/parts
                            if member.isdir():
                                dest.mkdir(exist_ok=True)
                                dest.chmod(0o755)
                            else:
                                total+=member.size
                                require(total<=512*1024**2,'release size limit')
                                with tar.extractfile(member) as src, dest.open('xb') as out:
                                    shutil.copyfileobj(src,out)
                                dest.chmod(0o755 if parts==Path('td-mta') else 0o644)
                    require((stage/'BUILD-INPUTS').is_file() and (stage/'notices').is_dir() and
                            digest(stage/'td-mta')==expected,'release inputs or hash')
                    durable(stage/'deployment-inputs.json',dict(revision=rev,tree=self.job['tree'],
                                                                binary_sha256=expected))
                    stage.rename(self.candidate.parent)
                    self.run(['sync','-f',self.candidate.parent])
                finally:
                    if stage.exists(): shutil.rmtree(stage)
            self.record['binary_sha256']=expected
        finally:
            as_user(['git','-C',repo,'worktree','remove','--force',source],timeout=120)
            as_user(['rm','-rf','--',prefix],timeout=120)

    def classify(self):
        args=['runuser','-u','td-mta','--',self.candidate,'postgres','schema-plan',
              '--connection-file',MAIL/'owner-connection']
        p=self.run(args,check=False,stdout=subprocess.PIPE,text=True)
        result=json.loads(p.stdout)
        if p.returncode==2 and result.get('error')=='invalid-arguments':
            # Releases predating schema-plan can only perform an exact-ledger deployment.
            self.config(self.candidate)
            return None
        require(p.returncode==0 and result.get('status')=='ok' and result['store_epoch']==self.epoch and
                result['current']==self.old_version and result['pending']==list(range(self.old_version+1,result['target']+1))
                and result['target']>=self.old_version,'unsupported ledger')
        return result

    def upgrade(self,ns=()):
        before=self.native(self.candidate,'schema-plan',ns=ns)
        require(before==self.plan,'migration plan changed')
        receipt=self.native(self.candidate,'schema-upgrade',ns=ns)
        require(receipt['store_epoch']==self.epoch and receipt['schema_version']==self.plan['target'],'upgrade receipt')
        require(self.plan['schema_usage']=='td_mta','schema grant namespace')
        grants=['BEGIN;']
        for table,privs in self.plan['primary_grants'].items():
            require(re.fullmatch('[a-z][a-z0-9_]*',table) and privs in LIMITS,'unsupported grant')
            grants.append(f'GRANT {privs} ON TABLE td_mta.{table} TO td_mta_runtime;')
        grants.append('COMMIT;')
        self.sql('owner','\n'.join(grants),ns=ns,script=True)
        self.config(self.candidate,ns)

    def active_timers(self):
        return sorted(line.split()[0] for line in self.out(['systemctl','list-units','--type=timer',
                       '--state=active','--no-legend','td-mta-*']).splitlines())

    def prepare_fence(self):
        self.timers=self.active_timers()
        require('td-mta-maint-backup.timer' in self.timers and 'td-mta-health.timer' in self.timers,'timer preflight')
        self.record['timers']=self.timers
        # Condition the writer services themselves so timer, dependency and boot
        # activation all respect a durable fence if the supervisor is interrupted.
        timer_units=[line.split()[0] for line in self.out(['systemctl','list-unit-files','--type=timer',
                     '--no-legend','td-mta-*.timer']).splitlines()]
        for u in sorted(set(UNITS+[x[:-6]+'.service' for x in timer_units])):
            d=Path('/etc/systemd/system')/(u+'.d')
            d.mkdir(mode=0o755,exist_ok=True)
            f=d/'98-td-mta-deploy-fence.conf'
            data=f'[Unit]\nConditionPathExists=!{FENCE}\n'
            if f.exists(): require(f.read_text()==data,'conflicting service fence')
            else:
                with f.open('x') as out: out.write(data)
                f.chmod(0o644)
            self.drops.append(f)
        if self.recovery:
            # On reboot nft state and /run are lost together. PID 1 must keep
            # normal PostgreSQL offline as long as the durable fence remains.
            self.boot_permit.touch(mode=0o600,exist_ok=True)
            directory=Path('/etc/systemd/system')/(PG+'.d')
            directory.mkdir(mode=0o755,exist_ok=True)
            condition=directory/'98-td-mta-deploy-boot.conf'
            data=f'[Service]\nExecCondition=+/usr/bin/test ! -e {FENCE} -o -e {self.boot_permit}\n'
            if condition.exists():
                require(condition.read_text()==data, 'conflicting PostgreSQL boot condition')
            else:
                with condition.open('x') as out:
                    out.write(data)
            condition.chmod(0o644)
            self.drops.append(condition)
            self.run(['sync','-f',condition])
        self.run(['systemctl','daemon-reload'])
        self.probes.start()

    def fence(self):
        self.event('fencing')
        require(not FENCE.exists(),'unexpected deployment fence')
        self.fenced=True
        with FENCE.open('x') as f:
            f.write(str(self.work)+'\n')
            f.flush()
            os.fsync(f.fileno())
        self.fenced=True
        self.run(['sync','-f',FENCE])
        self.run(['systemctl','stop',*self.timers])
        for t in self.timers:
            self.wait(lambda:self.out(['systemctl','show',t[:-6]+'.service','-p','ActiveState','--value'])
                      in ('inactive','failed'),30,'maintenance sessions')
        self.record['window_started_utc']=time.time()
        self.until=time.monotonic()+self.job['budget']
        self.run(['systemctl','stop',*UNITS])
        require(all(self.out(['systemctl','show',u,'-p','ActiveState','--value'])=='inactive' for u in UNITS),
                'mail fence')
        self.config(self.previous)
        if self.recovery:
            # Only the root-owned receiver and SQL operator may reconnect to
            # the production database until writer admission. Trial connections
            # use an independent network namespace.
            rules=(f'create table inet {self.table}\n'
                   f'add chain inet {self.table} output {{ type filter hook output priority -10; policy accept; }}\n'
                   f'add rule inet {self.table} output ip daddr 127.0.0.1 tcp dport 35432 meta skuid != 0 reject with tcp reset\n')
            self.firewall=True
            self.run(['nft','-f','-'],input=rules,text=True)
        self.check_source()
        require(self.sql('owner',"""SELECT NOT EXISTS(SELECT FROM pg_stat_activity WHERE datname IS NOT NULL
                AND pid<>pg_backend_pid()),NOT EXISTS(SELECT FROM pg_prepared_xacts)""")=='t|t','database writers')
        self.event('fenced')

    def wait(self,fn,seconds,reason):
        end=time.monotonic()+seconds
        while time.monotonic()<end:
            self.remaining()
            if fn(): return
            time.sleep(.2)
        raise RuntimeError(reason+' timeout')

    def copy_cold(self,warm):
        # The rollback directory stays on the primary device. Preseed it online;
        # compare contents after clean shutdown, including pages with equal mtimes.
        require(PGDATA.stat().st_dev==self.work.stat().st_dev,'primary rollback device')
        self.cold.mkdir(mode=0o700,exist_ok=True)
        require(not any(p.is_symlink() for p in PGDATA.rglob('*')),'external PGDATA symlink')
        mounts=self.out(['findmnt','--noheadings','--raw','--output','TARGET']).splitlines()
        require(not any(Path(m).is_relative_to(PGDATA) for m in mounts),'mounted PGDATA subtree')
        size=int(self.out(['du','--apparent-size','-s','-B1',PGDATA]).split()[0])
        if warm:
            require(shutil.disk_usage(PGDATA).free>size+2*GIB,'primary preseed space')
        args=['rsync','-ax','--numeric-ids','--delete','--checksum','--fsync',
              str(PGDATA)+'/',str(self.cold)+'/']
        end=time.monotonic()+min(900,self.remaining())
        with subprocess.Popen(args,env=self.base_env,start_new_session=True,stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL) as copy:
            try:
                while copy.poll() is None:
                    require(shutil.disk_usage(PGDATA).free>2*GIB,'primary disk reserve')
                    require(time.monotonic()<end,'primary copy timeout')
                    time.sleep(.15)
            finally:
                if copy.poll() is None:
                    os.killpg(copy.pid,signal.SIGKILL)
                    copy.wait()
            require(copy.returncode==0 or (warm and copy.returncode==24),'preparing primary data copy')

    def migrate(self):
        self.recovery.seal()
        self.event('exact recovery point verified')
        self.pg_stopped=True
        self.run(['systemctl','stop',PG])
        require('shut down'==self.control_state(PGDATA),'original primary clean shutdown')
        self.copy_cold(False)
        require(not any(p.is_symlink() for p in self.cold.rglob('*')), 'external cold copy pathname')
        require(self.control_state(self.cold)=='shut down' and not (self.cold/'postmaster.pid').exists(),
                'candidate data copy consistency')
        # Qualify the actual data and grants behind an isolated TCP listener.
        # The clean original stays available even after the candidate commits.
        unit='td-mta-'+self.work.name+'-candidate-db'
        try:
            self.run(['systemd-run','--quiet','--collect','--unit='+unit,'--property=Type=exec',
                      '--property=BindsTo=td-mta-'+self.work.name+'.service',
                      '--property=After=td-mta-'+self.work.name+'.service',
                      '--property=User=td-mta','--property=Group=td-mta','--property=PrivateNetwork=yes',
                      '--property=PrivateIPC=yes','--property=ProtectSystem=strict','--property=ProtectHome=yes',
                      '--property=NoNewPrivileges=yes','--property=KillSignal=SIGINT',
                      f'--property=BindPaths={self.cold}:{PGDATA}',f'--property=ReadWritePaths={PGDATA}',
                      TOOLS/'postgres','-D',PGDATA,'-c','config_file='+str(MAIL/'postgresql.conf'),
                      '-c','data_directory='+str(PGDATA),
                      '-c','archive_mode=off','-c','primary_conninfo=','-c','unix_socket_directories='])
            pid=self.out(['systemctl','show',unit,'-p','MainPID','--value'])
            require(pid.isdecimal() and os.stat('/proc/'+pid+'/ns/net').st_ino!=os.stat('/proc/self/ns/net').st_ino,
                    'candidate database namespace')
            ns=['nsenter','--target',pid,'--net','--']
            self.wait(lambda:self.ready_db(ns),15,'private candidate database startup')
            self.event('migrating fenced candidate database')
            self.upgrade(ns)
        finally:
            # Cleanup must finish even when the cutover budget expired.
            limit,self.until=self.until,None
            try: self.run(['systemctl','stop',unit],check=False,timeout=120)
            finally: self.until=limit
        require(self.control_state(self.cold)=='shut down','candidate shutdown')
        PGDATA.rename(self.original)
        self.cold.rename(PGDATA)
        self.run(['sync','-f',PGDATA])
        self.run(['systemctl','start',PG])
        self.wait(self.ready_db,15,'candidate database startup')
        self.check_source()
        require(self.sql('owner','SELECT max(version) FROM td_mta.schema_migrations')==str(self.plan['target']),
                'live target ledger')

    def control_state(self,path):
        rows=self.out([TOOLS/'pg_controldata',path]).splitlines()
        return next((s.split(':',1)[1].strip() for s in rows if s.startswith('Database cluster state:')),'')

    def switch(self,bin):
        link=ACTIVE.parent/('.'+self.work.name)
        link.symlink_to(bin)
        link.replace(ACTIVE)
        self.run(['sync','-f',ACTIVE.parent])

    def admit(self):
        self.event('opening writers')
        self.remaining()
        if self.firewall:
            # A lost netlink acknowledgement is ambiguous. Roll back only if
            # both fences can still be positively observed.
            self.admitting=True
            try:
                self.run(['nft','delete','table','inet',self.table])
            except BaseException:
                self.until=None
                if FENCE.exists() and self.table in self.nft_tables():
                    self.admitting=False
                raise
            self.firewall=False
        previously_exposed=self.admitting
        self.admitting=True
        try:
            FENCE.unlink(missing_ok=True)
        except BaseException:
            if not previously_exposed and FENCE.exists():
                self.admitting=False
            raise
        self.run(['sync','-f',OP])
        self.run(['systemctl','start',*UNITS])
        # Timers must return even while ACME waits for its 180-second lease.
        self.run(['systemctl','start','td-mta-maint-backup.timer'])
        self.run(['systemctl','start',*self.timers])
        self.fenced=False
        self.wait(lambda:all(self.probes.samples[k] and self.probes.samples[k][-1][1]
                             and self.probes.samples[k][-1][0]>=self.record['events'][-1][0]
                             for k in ('smtp','jmap')),20,'listener recovery')
        self.record['listeners_up_utc']=time.time()
        for name,samples in self.probes.samples.items():
            failures=[i for i,(_,ok) in enumerate(samples) if not ok]
            if failures and failures[0]>0 and failures[-1]+1<len(samples):
                first,last=failures[0],failures[-1]
                self.record[name+'_interruption_bounds_seconds']=[
                    round(samples[last][0]-samples[first][0],3),
                    round(samples[last+1][0]-samples[first-1][0],3)]
        self.record['window_seconds']=round(time.monotonic()-(self.until-self.job['budget']),3) if self.until is not None else None
        self.until=None

    def nft_tables(self):
        return {s.split()[2] for s in self.out(['nft','list','tables']).splitlines()
                if s.startswith('table inet ') and len(s.split())==3}

    def finish_admission(self):
        # Converge on the selected database and binary even if the window timer
        # expired halfway through a systemd request.
        if self.firewall and self.table in self.nft_tables():
            self.run(['nft','delete','table','inet',self.table])
        self.firewall=False
        if FENCE.exists():
            require(FENCE.read_text().strip()==str(self.work),'foreign deployment fence')
            FENCE.unlink()
            self.run(['sync','-f',OP])
        for group in (UNITS,['td-mta-maint-backup.timer'],self.timers):
            self.run(['systemctl','start',*group],check=False,timeout=120)
        self.fenced=False
        self.wait(self.acme_online,240,'ACME lease after interrupted admission')
        self.healthy(ACTIVE.resolve(strict=True))
        require(self.active_timers()==self.timers,'timer recovery')
        self.record['recovery_status']='selected release healthy after interrupted admission'

    def rollback(self):
        self.until=None
        if not self.fenced or self.admitting: return
        self.event('restoring original primary before writer admission')
        if self.pg_stopped:
            self.run(['systemctl','stop',PG],timeout=120)
            if self.original.exists():
                if PGDATA.exists():
                    PGDATA.rename(self.work/'unreleased-pgdata')
                self.original.rename(PGDATA)
                self.run(['sync','-f',PGDATA])
            self.run(['systemctl','start',PG])
            self.wait(self.ready_db,25,'original primary restart')
        if self.firewall:
            if self.table in self.nft_tables():
                try:
                    self.run(['nft','delete','table','inet',self.table])
                except RuntimeError:
                    if self.table in self.nft_tables():
                        raise
            self.firewall=False
        self.config(self.previous)
        self.switch(self.previous)
        self.admit()
        self.wait(self.acme_online,240,'restored ACME lease')
        self.healthy(self.previous)
        self.record['rollback']='original release and database verified'

    def execute(self):
        require(not FENCE.exists(),'unfinished deployment fence: inspect previous receipt')
        require(ACTIVE.resolve(strict=True)==self.previous,'production release changed')
        self.check_source()
        old=self.native(self.previous,'schema-inspect')
        self.epoch=old['store_epoch']
        self.old_version=old['schema_version']
        self.selected=self.sql('owner',"SELECT encode(version_id,'hex') FROM td_mta.config_selection")
        require(re.fullmatch('[0-9a-f]{32}',self.selected),'selected configuration')
        self.config(self.previous)
        self.healthy(self.previous)
        if self.previous==self.candidate:
            for name in self.probes.samples: self.probes.probe(name)
            self.event('already deployed and healthy')
            return
        self.event('building pinned revision')
        self.build()
        self.plan=self.classify()
        migration=bool(self.plan and self.plan['pending'])
        if self.plan and not migration:
            cmd=['runuser','-u','td-mta','--',self.candidate,'postgres','runtime-check',
                 '--connection-file',MAIL/'runtime-connection']
            check=self.run(cmd,check=False,stdout=subprocess.PIPE,text=True)
            if check.returncode:
                require(json.loads(check.stdout).get('error')=='incorrect-data-privileges',
                        'runtime candidate preflight')
                migration=True
        self.record.update(schema_before=self.old_version,epoch=self.epoch,config=self.selected,
                           schema_after=self.plan['target'] if self.plan else self.old_version)
        try:
            if migration:
                require(shutil.disk_usage(PGDATA).free>2*GIB+
                        int(self.sql('owner','SELECT sum(pg_database_size(oid)) FROM pg_database')),
                        'primary rollback space')
                self.recovery=Recovery(self)
                self.record['recovery']=str(self.recovery.work)
                self.event('online backup and migration rehearsal')
                self.recovery.prepare()
                self.copy_cold(True)
            else:
                self.config(self.candidate)
            self.prepare_fence()
            self.fence()
            if migration:
                self.migrate()
            else:
                self.config(self.candidate)
            self.switch(self.candidate)
            self.admit()
            self.wait(lambda:self.acme_online(),240,'ACME lease recovery')
            self.config(self.candidate)
            self.healthy(self.candidate)
            require(self.active_timers()==self.timers,'timer set changed')
            self.event('verified deployment')
        except BaseException:
            self.until=None
            try:
                if not self.admitting:
                    self.rollback()
            finally:
                if self.admitting:
                    self.finish_admission()
            raise
        finally:
            self.until=None
            self.probes.finish()
            if 'window_started_utc' in self.record:
                self.record['recovery_finished_utc']=time.time()
                for name,samples in self.probes.samples.items():
                    failed=[i for i,(_,ok) in enumerate(samples) if not ok]
                    if failed:
                        first,last=failed[0],failed[-1]
                        self.record[name+'_outage']={
                            'first_failure':samples[first][0],
                            'last_failure':samples[last][0],
                            'last_success_before':samples[first-1][0] if first else None,
                            'first_success_after':samples[last+1][0] if last+1<len(samples) else None}
                durable(self.work/'receipt.json',self.record)
            try:
                if self.recovery:
                    self.recovery.close()
            finally:
                if not FENCE.exists():
                    if self.cold.exists():
                        shutil.rmtree(self.cold)
                    self.boot_permit.unlink(missing_ok=True)
                    for f in self.drops:
                        f.unlink(missing_ok=True)
                    if self.drops:
                        self.run(['systemctl','daemon-reload'])

    def acme_online(self):
        pid=self.out(['systemctl','show',UNITS[-1],'-p','MainPID','--value'])
        return pid!='0' and 'pid='+pid+',' in self.out(['ss','-H','-ltnp','sport = :80'])


def main():
    os.umask(0o077)
    with zipfile.ZipFile(sys.argv[0]) as code:
        job=json.loads(code.read('job.json'))
    h=Deployment(job)
    with (OP/'activation.lock').open('a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        def interrupted(signum, frame):
            signal.signal(signal.SIGTERM,signal.SIG_IGN)
            signal.signal(signal.SIGINT,signal.SIG_IGN)
            raise RuntimeError('deployment interrupted')
        signal.signal(signal.SIGTERM,interrupted)
        signal.signal(signal.SIGINT,interrupted)
        try:
            h.execute()
        except BaseException as error:
            h.record['error']=str(error) if isinstance(error,RuntimeError) else type(error).__name__
            h.event('deployment failed')
            raise SystemExit(1)


if __name__=='__main__':
    main()
