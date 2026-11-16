// The main screens at widths from a small phone to a wide desktop window, by mouse and by finger,
// at large system text sizes, with no overflow and no layout error. Nothing is compared with
// PNGs, so it runs with the other widget tests.

import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

/// A window: its width and whether it is Windows (mouse) or Android (touch).
typedef Window = ({double width, bool windows});

/// Phones and tablets by touch, a narrow to wide desktop window by mouse, across both size class
/// boundaries.
const windows = <Window>[
  (width: 320, windows: false),
  (width: 390, windows: false),
  (width: 600, windows: false),
  (width: 840, windows: false),
  (width: 420, windows: true),
  (width: 700, windows: true),
  (width: 1000, windows: true),
  (width: 1600, windows: true),
];

/// System text sizes: default, a common larger setting, and the largest the app follows.
const textScales = [1.0, 1.3, 2.0];

const _height = 900.0;

/// Opens the session strip, then the stats panel.
Future<void> _openStats(WidgetTester tester) async {
  await tester.tap(find.byType(EdgeTab));
  await tester.pumpAndSettle();
  final stats = find.descendant(of: find.byType(EdgeStrip), matching: find.text('Stats'));
  await tester.ensureVisible(stats);
  await tester.pumpAndSettle();
  await tester.tap(stats);
  await tester.pumpAndSettle();
}

void main() {
  final scenes = <String, Scene>{
    for (final name in [
      'first_run',
      'enroll_created',
      'enroll_unlock',
      'enroll_refused',
      'home',
      'home_empty',
      'home_problems',
      'home_with_host',
      'pair_enter',
      'pair_enter_remote_error',
      'pair_waiting',
      'pair_approved_pc',
      'pair_timed_out',
      'approval',
      'session_placeholder',
    ])
      name: Scene(contractState(name)),
    'session_stats': Scene(contractState('session'), setUp: _openStats),
  };
  for (final MapEntry(key: name, value: scene) in scenes.entries) {
    for (final window in windows) {
      for (final scale in textScales) {
        final kind = window.windows ? 'mouse' : 'touch';
        testWidgets('$name at ${window.width.toInt()} px by $kind, text x$scale', (tester) async {
          tester.platformDispatcher.textScaleFactorTestValue = scale;
          addTearDown(tester.platformDispatcher.clearTextScaleFactorTestValue);

          final errors = <String>[];
          final previous = FlutterError.onError;
          FlutterError.onError = (details) {
            final at = RegExp(r'(lib/[a-z_/]+\.dart:\d+)').firstMatch(details.toString())?.group(1);
            errors.add(
              '${details.exceptionAsString().split('\n').first}${at == null ? '' : ' at $at'}',
            );
          };
          addTearDown(() => FlutterError.onError = previous);

          await pumpApp(
            tester,
            scene.state,
            size: Size(window.width, _height),
            windows: window.windows,
          );
          // Spinners and countdowns never settle; let entrance animations finish.
          await tester.pump(const Duration(seconds: 1));
          await scene.setUp?.call(tester);
          FlutterError.onError = previous;
          expect(errors, isEmpty, reason: errors.toSet().join('\n'));
        });
      }
    }
  }
}
