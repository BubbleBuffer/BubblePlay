import 'dart:convert';
import 'dart:typed_data';

import 'package:bubbleplay_app/src/screens/monitor_preview_overlay.dart';
import 'package:bubbleplay_app/src/session/monitor_previews.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

/// A 1x1 PNG: any decodable picture will do for the widget.
final Uint8List _picture = base64Decode(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
);

class FakePreviews implements MonitorPreviewSource {
  final Map<String, MonitorPreview> held = {};
  final List<String> taken = [];
  @override
  int liveGeneration = 4;

  void hold(String selector, {int width = 640, int height = 360}) {
    held[selector] = MonitorPreview(
      monitor: selector,
      width: width,
      height: height,
      jpeg: _picture,
    );
  }

  @override
  MonitorPreview? take(String selector) {
    taken.add(selector);
    return held.remove(selector);
  }
}

/// Pumps the overlay (800x600, with a tap target underneath) for [selected]. Pumping again with
/// another selector is a monitor switch on the same state.
Future<void> pumpOverlay(
  WidgetTester tester,
  MonitorPreviewSource source,
  String? selected, {
  Duration maxWait = const Duration(seconds: 3),
  VoidCallback? onTapBelow,
}) {
  return tester.pumpWidget(
    Directionality(
      textDirection: TextDirection.ltr,
      child: Center(
        child: SizedBox(
          width: 800,
          height: 600,
          child: Stack(
            children: [
              GestureDetector(onTap: onTapBelow, child: const SizedBox.expand()),
              Positioned.fill(
                child: MonitorPreviewOverlay(
                  source: source,
                  selected: selected,
                  maxWait: maxWait,
                ),
              ),
            ],
          ),
        ),
      ),
    ),
  );
}

double opacityNow(WidgetTester tester) =>
    tester.widget<FadeTransition>(find.byType(FadeTransition)).opacity.value;

void main() {
  testWidgets('a start on a monitor takes and shows nothing', (tester) async {
    final source = FakePreviews()..hold('DISPLAY1');
    await pumpOverlay(tester, source, 'DISPLAY1');
    expect(source.taken, isEmpty);
    expect(find.byType(Image), findsNothing);
  });

  testWidgets('a switch to a monitor with a held picture shows it at once, at its aspect',
      (tester) async {
    final source = FakePreviews()..hold('DISPLAY2', width: 640, height: 360);
    await pumpOverlay(tester, source, 'DISPLAY1');
    await pumpOverlay(tester, source, 'DISPLAY2');
    expect(source.taken, ['DISPLAY2']);
    final image = tester.widget<Image>(find.byType(Image));
    expect(image.gaplessPlayback, isTrue);
    expect(opacityNow(tester), 1);
    final size = tester.getSize(find.byType(Image));
    expect(size.width / size.height, closeTo(640 / 360, 0.01));
    expect(size.width, 800);
    // The picture was used: it is no longer held.
    expect(source.held, isEmpty);
  });

  testWidgets('a switch without a held picture does nothing', (tester) async {
    final source = FakePreviews();
    await pumpOverlay(tester, source, 'DISPLAY1');
    await pumpOverlay(tester, source, 'DISPLAY2');
    expect(source.taken, ['DISPLAY2']);
    expect(find.byType(Image), findsNothing);
  });

  testWidgets('the picture stays while the generation is unchanged', (tester) async {
    final source = FakePreviews()..hold('DISPLAY2');
    await pumpOverlay(tester, source, 'DISPLAY1');
    await pumpOverlay(tester, source, 'DISPLAY2');
    await tester.pump(const Duration(milliseconds: 1000));
    expect(find.byType(Image), findsOneWidget);
    expect(opacityNow(tester), 1);
  });

  testWidgets('the first live frame of a new generation fades it out in about 200 ms',
      (tester) async {
    final source = FakePreviews()..hold('DISPLAY2');
    await pumpOverlay(tester, source, 'DISPLAY1');
    await pumpOverlay(tester, source, 'DISPLAY2');

    source.liveGeneration = 5;
    // The poll notices within 50 ms and the fade starts.
    await tester.pump(const Duration(milliseconds: 60));
    await tester.pump(const Duration(milliseconds: 100));
    expect(opacityNow(tester), inExclusiveRange(0.0, 1.0));
    await tester.pump(const Duration(milliseconds: 150));
    await tester.pump();
    expect(find.byType(Image), findsNothing);
    expect(find.byType(FadeTransition), findsNothing);
  });

  testWidgets('it fades without a new generation after the longest wait', (tester) async {
    final source = FakePreviews()..hold('DISPLAY2');
    const wait = Duration(seconds: 1);
    await pumpOverlay(tester, source, 'DISPLAY1', maxWait: wait);
    await pumpOverlay(tester, source, 'DISPLAY2', maxWait: wait);
    expect(find.byType(Image), findsOneWidget);
    await tester.pump(const Duration(milliseconds: 1100));
    await tester.pump(const Duration(milliseconds: 300));
    expect(find.byType(Image), findsNothing);
  });

  testWidgets('a second switch replaces the picture at once', (tester) async {
    final source = FakePreviews()
      ..hold('DISPLAY2')
      ..hold('DISPLAY3', width: 320, height: 240);
    await pumpOverlay(tester, source, 'DISPLAY1');
    await pumpOverlay(tester, source, 'DISPLAY2');
    source.liveGeneration = 5;
    await tester.pump(const Duration(milliseconds: 60));
    await tester.pump(const Duration(milliseconds: 100));
    expect(opacityNow(tester), lessThan(1));

    await pumpOverlay(tester, source, 'DISPLAY3');
    expect(opacityNow(tester), 1);
    final size = tester.getSize(find.byType(Image));
    expect(size.width / size.height, closeTo(320 / 240, 0.01));
    // The new picture waits for a generation after the one it started with.
    await tester.pump(const Duration(milliseconds: 500));
    expect(find.byType(Image), findsOneWidget);
    source.liveGeneration = 6;
    await tester.pump(const Duration(milliseconds: 60));
    await tester.pump(const Duration(milliseconds: 250));
    await tester.pump();
    expect(find.byType(Image), findsNothing);
  });

  testWidgets('a source without the feature shows nothing', (tester) async {
    await pumpOverlay(tester, const NoMonitorPreviews(), 'DISPLAY1');
    await pumpOverlay(tester, const NoMonitorPreviews(), 'DISPLAY2');
    await tester.pump(const Duration(seconds: 1));
    expect(find.byType(Image), findsNothing);
    expect(const NoMonitorPreviews().take('DISPLAY2'), isNull);
    expect(const NoMonitorPreviews().liveGeneration, 0);
  });

  testWidgets('the overlay does not take pointer input', (tester) async {
    final source = FakePreviews()..hold('DISPLAY2');
    var taps = 0;
    await pumpOverlay(tester, source, 'DISPLAY1', onTapBelow: () => taps++);
    await pumpOverlay(tester, source, 'DISPLAY2', onTapBelow: () => taps++);
    expect(find.byType(Image), findsOneWidget);
    await tester.tapAt(const Offset(400, 300));
    expect(taps, 1);
  });
}
