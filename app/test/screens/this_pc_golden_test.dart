@Tags(['golden'])
library;

import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/devices_fixtures.dart';

/// One golden per state, dark and light, at desktop width (This PC is Windows only and the phone
/// never shows it). Files land in `goldens/this_pc_<state>_desktop_<theme>.png`, written on the CI
/// platform (Linux), never on a developer PC.
void main() {
  final scenes = <String, List<Map<String, Object?>>>{
    'idle': connectedHost(),
    'pairing': [...connectedHost(), pairWaitingMsg()],
    'request': [...connectedHost(), pairWaitingMsg(), pairRequestMsg()],
    'done': [
      ...connectedHost(),
      {'type': 'pairDone', 'id': 'dev-new', 'name': 'Galaxy Tab'},
    ],
    'denied': [
      ...connectedHost(),
      {'type': 'pairDenied'},
    ],
    'save_error': [
      ...connectedHost(),
      {'type': 'saveError', 'message': 'The trusted list could not be written.'},
    ],
    'no_host': [
      {'type': 'disconnected', 'reason': 'hostNotRunning'},
    ],
    'not_trusted': [
      {'type': 'disconnected', 'reason': 'notTrusted'},
    ],
    'retrying': [
      {'type': 'disconnected', 'reason': 'io'},
    ],
  };
  for (final MapEntry(key: name, value: messages) in scenes.entries) {
    for (final brightness in Brightness.values) {
      final golden = 'this_pc_${name}_desktop_${brightness.name}';
      testWidgets('golden $golden', (tester) async {
        await pumpThisPc(tester, messages, brightness: brightness);
        // Tall enough for the pairing view (code, QR, countdown, Cancel) without scrolling.
        tester.view.physicalSize = const Size(1000, 960);
        await tester.pump();
        await expectLater(find.byType(WidgetsApp), matchesGoldenFile('goldens/$golden.png'));
      });
    }
  }

  // The revoke confirmation is a modal over the list.
  for (final brightness in Brightness.values) {
    final golden = 'this_pc_revoke_desktop_${brightness.name}';
    testWidgets('golden $golden', (tester) async {
      await pumpThisPc(tester, connectedHost(), brightness: brightness);
      await tester.tap(find.text('Revoke').first);
      await tester.pumpAndSettle();
      await expectLater(find.byType(WidgetsApp), matchesGoldenFile('goldens/$golden.png'));
    });
  }
}
