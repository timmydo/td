"""Failure boundaries that must hold without access to a production host."""
import unittest
from unittest.mock import Mock, patch

import activate


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
