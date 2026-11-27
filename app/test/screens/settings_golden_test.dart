@Tags(['golden'])
library;

import 'package:flutter/widgets.dart';
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
      await tester.pumpAndSettle();
      await tester.enterText(find.byType(EditableText), 'pw');
      await tester.pump();
      await tester.tap(find.text('Export').last);
      await tester.pump();
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 400));
    }),
  });
}
