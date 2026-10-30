@Tags(['golden'])
library;

import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

void main() {
  goldenScenes('home', {
    'list': contractState('home'),
    'empty': contractState('home_empty'),
    'problems': contractState('home_problems'),
    'host': contractState('home_with_host'),
  });
}
