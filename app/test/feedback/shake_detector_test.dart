import 'dart:async';

import 'package:bubbleplay_app/src/feedback/shake_detector.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

final _t0 = DateTime.utc(2026, 11, 2, 12);

ShakeSample _rest(int ms) => ShakeSample(0, 0, 9.80665, _t0.add(Duration(milliseconds: ms)));

ShakeSample _jolt(int ms) => ShakeSample(20, 15, 12, _t0.add(Duration(milliseconds: ms)));

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  group('ShakeDetector', () {
    late int shakes;
    late ShakeDetector detector;
    setUp(() {
      shakes = 0;
      detector = ShakeDetector(() => shakes++);
    });

    test('rest and a single bump are not a shake', () {
      for (var i = 0; i < 100; i++) {
        detector.add(_rest(i * 20));
      }
      detector.add(_jolt(2100));
      expect(shakes, 0);
    });

    test('three jolts inside the window are a shake', () {
      detector.add(_jolt(0));
      detector.add(_jolt(300));
      expect(shakes, 0);
      detector.add(_jolt(600));
      expect(shakes, 1);
    });

    test('jolts spread over more than the window are not', () {
      detector.add(_jolt(0));
      detector.add(_jolt(600));
      detector.add(_jolt(1300));
      expect(shakes, 0);
    });

    test('one swing reported many times is one jolt', () {
      for (var i = 0; i < 6; i++) {
        detector.add(_jolt(i * 20));
      }
      expect(shakes, 0);
    });

    test('debounce: a second shake right after is ignored, one later is not', () {
      for (final ms in [0, 300, 600]) {
        detector.add(_jolt(ms));
      }
      for (final ms in [900, 1200, 1500]) {
        detector.add(_jolt(ms));
      }
      expect(shakes, 1);
      for (final ms in [3000, 3300, 3600]) {
        detector.add(_jolt(ms));
      }
      expect(shakes, 2);
    });

    test('reset forgets earlier jolts', () {
      detector.add(_jolt(0));
      detector.add(_jolt(300));
      detector.reset();
      detector.add(_jolt(600));
      expect(shakes, 0);
    });
  });

  group('ShakeService', () {
    testWidgets('listens only in the foreground, honours allowed and stops on dispose', (tester) async {
      var opened = 0;
      var cancelled = 0;
      late StreamController<ShakeSample> controller;
      Stream<ShakeSample> open() {
        opened++;
        controller = StreamController<ShakeSample>(onCancel: () => cancelled++);
        return controller.stream;
      }

      var shakes = 0;
      var allowed = true;
      final service = ShakeService(
        samples: open,
        onShake: () => shakes++,
        allowed: () => allowed,
      );
      expect(service.listening, isTrue);
      expect(opened, 1);

      Future<void> shake(int base) async {
        for (final ms in [0, 300, 600]) {
          controller.add(_jolt(base + ms));
        }
        await tester.pump();
      }

      await shake(0);
      expect(shakes, 1);

      allowed = false; // a remote session is showing
      await shake(5000);
      expect(shakes, 1);
      allowed = true;

      tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.paused);
      expect(service.listening, isFalse);
      expect(cancelled, 1);
      tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.resumed);
      expect(service.listening, isTrue);
      expect(opened, 2);
      await shake(10000);
      expect(shakes, 2);

      service.dispose();
      expect(service.listening, isFalse);
      expect(cancelled, 2);
    });

    test('without a stream it does nothing', () {
      final service = ShakeService(samples: null, onShake: () => fail('no shake'));
      expect(service.listening, isFalse);
      service.dispose();
    });
  });
}
