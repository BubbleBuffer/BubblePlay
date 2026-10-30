@Tags(['golden'])
library;

import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

void main() {
  goldenScenes('enroll', {
    'first_run': contractState('first_run'),
    'created': contractState('enroll_created'),
    'unlock': contractState('enroll_unlock'),
    'unlock_clean': contractState('enroll_unlock_clean'),
    'refused': contractState('enroll_refused'),
  });
}
