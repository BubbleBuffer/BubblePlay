@Tags(['golden'])
library;

import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

void main() {
  goldenScenes('pairing', {
    'pair_enter': contractState('pair_enter'),
    'pair_enter_remote_error': contractState('pair_enter_remote_error'),
    'pair_waiting': contractState('pair_waiting'),
    'pair_approved_pc': contractState('pair_approved_pc'),
    'pair_approved_device': contractState('pair_approved_device'),
    'pair_denied': contractState('pair_denied'),
    'pair_timed_out': contractState('pair_timed_out'),
    'approval': contractState('approval'),
  });
}
