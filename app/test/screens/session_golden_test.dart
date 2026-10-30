@Tags(['golden'])
library;

import 'package:bb_ui/bb_ui.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

/// Taps the tab so the strip is open in the picture.
Future<void> openStrip(WidgetTester tester) async {
  await tester.tap(find.byType(EdgeTab));
  await tester.pumpAndSettle();
}

/// Opens the stats panel from the open strip.
Future<void> openStats(WidgetTester tester) async {
  await openStrip(tester);
  final stats = find.descendant(of: find.byType(EdgeStrip), matching: find.text('Stats'));
  await tester.ensureVisible(stats);
  await tester.pumpAndSettle();
  await tester.tap(stats);
  await tester.pumpAndSettle();
}

void main() {
  goldenScenes('session', {
    'closed': contractState('session'),
    'strip': Scene(contractState('session'), setUp: openStrip),
    'stats': Scene(contractState('session'), setUp: openStats),
    'placeholder': Scene(contractState('session_placeholder'), setUp: openStrip),
  });
}
