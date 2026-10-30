@Tags(['golden'])
library;

import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

/// Opens Settings from the PC list.
Future<void> openSettings(WidgetTester tester) async {
  await tester.tap(find.text('Settings'));
  await tester.pumpAndSettle();
}

void main() {
  goldenScenes('settings', {
    'panel': Scene(contractState('home'), setUp: openSettings),
    'exported': Scene(contractState('home'), setUp: (tester) async {
      await openSettings(tester);
      await tester.tap(find.text('Export'));
      await tester.pump();
      await tester.pump();
    }),
  });
}
