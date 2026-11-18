"""Release selection and failure boundaries without access to a production host."""
from contextlib import redirect_stderr
import io
from pathlib import Path
import runpy
import subprocess
import sys
import unittest
from unittest.mock import Mock, patch

import activate


class Selection(unittest.TestCase):
    def setUp(self):
        launcher = runpy.run_path(str(Path(__file__).resolve().parents[1]/'deploy'))
        self.select = launcher['select']
        self.main = launcher['main']
        self.live = '1'*40
        self.tip = '2'*40

    def test_skip_selects_main_after_checking_ancestry(self):
        check = Mock()
        with patch.dict(self.select.__globals__, run=check, Path=Mock(side_effect=AssertionError)):
            self.assertEqual(self.select(self.tip, self.live, skip_tests=True), (self.tip, None))
        check.assert_called_once_with(['git', 'merge-base', '--is-ancestor', self.live, self.tip])

    def test_skip_rejects_divergent_history(self):
        check = Mock(side_effect=subprocess.CalledProcessError(1, 'git merge-base'))
        with patch.dict(self.select.__globals__, run=check), self.assertRaises(subprocess.CalledProcessError):
            self.select(self.tip, self.live, skip_tests=True)

    def test_cli_requires_full_override_spelling(self):
        for abbreviated in ('--s', '--skip'):
            with self.subTest(abbreviated=abbreviated), patch.object(sys, 'argv', ['deploy', abbreviated]):
                with patch.dict(self.main.__globals__, output=Mock(side_effect=AssertionError)):
                    with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as stopped:
                        self.main()
                self.assertEqual(stopped.exception.code, 2)

    def test_cli_pins_advertised_main_for_both_modes(self):
        class Selected(Exception):
            pass

        for args in ([], ['--skip-tests']):
            with self.subTest(args=args):
                read = Mock(side_effect=[self.tip+'\trefs/heads/main', '3'*40, '',
                                         '/opt/td-mta/lib/td-mta/releases/git-'+self.live+'/td-mta'])
                run = Mock()
                select = Mock(side_effect=Selected)
                with patch('subprocess.run', return_value=subprocess.CompletedProcess([], 0)) as ancestry:
                    with patch.object(sys, 'argv', ['deploy', *args]), self.assertRaises(Selected):
                        with patch.dict(self.main.__globals__, output=read, run=run, select=select):
                            self.main()
                ancestry.assert_called_once_with(['git', 'merge-base', '--is-ancestor', '3'*40, self.tip],
                                                 cwd=self.main.__globals__['ROOT'])
                self.assertEqual(read.call_args_list[0].args[0],
                                 ['git','ls-remote','--exit-code','--refs','origin','refs/heads/main'])
                run.assert_any_call(['git', 'fetch', '--no-write-fetch-head', '--no-tags', 'origin', self.tip])
                select.assert_called_once_with(self.tip, self.live, skip_tests=bool(args))


class Admission(unittest.TestCase):
    def setUp(self):
        self.h = activate.Deployment.__new__(activate.Deployment)
        self.h.work = activate.Path('/var/lib/td-mta-operator/deploy-test')
        self.h.table = 'td_mta_deploy_test'
        self.h.fenced = True
        self.h.firewall = True
        self.h.admitting = False
        self.h.pg_stopped = False
        self.h.until = None
        self.h.previous = self.h.work/'test-binary'
        self.h.timers = ['td-mta-maint-backup.timer']
        self.h.event = Mock()
        self.h.run = Mock()
        self.h.nft_tables = Mock()
        self.fence = Mock()
        self.fence.exists.return_value = True
        self.fence.read_text.return_value = str(self.h.work)
        patcher = patch.object(activate, 'FENCE', self.fence)
        patcher.start()
        self.addCleanup(patcher.stop)

    def deletion_fault(self, present):
        self.h.run.side_effect = RuntimeError('lost acknowledgement')
        self.h.nft_tables.return_value = {self.h.table} if present else set()
        with self.assertRaisesRegex(RuntimeError, 'lost acknowledgement'):
            self.h.admit()

    def test_database_and_service_fences_still_present(self):
        self.deletion_fault(True)
        self.assertFalse(self.h.admitting)
        self.fence.unlink.assert_not_called()

    def test_lost_ack_after_database_exposure_preserves_selected_data(self):
        self.deletion_fault(False)
        self.assertTrue(self.h.admitting)
        self.h.run.reset_mock()
        self.h.rollback()
        self.h.run.assert_not_called()

    def test_cleanup_handles_ambiguous_rule_install_before_reopening(self):
        for present in (False, True):
            with self.subTest(present=present):
                self.h.firewall = True
                self.h.nft_tables.return_value = {self.h.table} if present else set()
                self.h.run.reset_mock()
                self.h.config = Mock()
                self.h.switch = Mock()
                self.h.admit = Mock()
                self.h.acme_online = Mock(return_value=True)
                self.h.healthy = Mock()
                self.h.record = {}
                self.h.rollback()
                self.assertEqual(self.h.run.called, present)
                self.h.config.assert_called_once_with(self.h.previous)
                self.h.admit.assert_called_once()

    def test_interrupted_service_start_converges_without_rewinding(self):
        self.h.record = {}
        self.h.admitting = True
        self.h.nft_tables.return_value = set()
        self.h.wait = Mock()
        self.h.healthy = Mock()
        self.h.active_timers = Mock(return_value=self.h.timers)
        with patch.object(activate, 'ACTIVE') as selected:
            self.h.finish_admission()
            self.h.healthy.assert_called_once_with(selected.resolve.return_value)
        calls = [c.args[0] for c in self.h.run.call_args_list]
        self.assertIn(['systemctl', 'start', *activate.UNITS], calls)
        self.assertIn(['systemctl', 'start', *self.h.timers], calls)
        self.fence.unlink.assert_called_once()
        self.assertFalse(self.h.fenced)


if __name__ == '__main__':
    unittest.main()
