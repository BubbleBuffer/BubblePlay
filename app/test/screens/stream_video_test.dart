import 'package:bubbleplay_app/src/session/stream_channel.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

/// A stream channel that records what the screen sends and answers [texture].
class RecordingStream implements StreamChannel {
  int? texture;
  final rects = <List<double>>[];
  final pointers = <StreamPointer>[];
  final keys = <List<Object>>[];

  @override
  Future<int?> textureId() async => texture;

  @override
  Future<void> setVideoRect(double x, double y, double width, double height, double ratio) async =>
      rects.add([x, y, width, height, ratio]);

  @override
  Future<void> pointer(StreamPointer event) async => pointers.add(event);

  @override
  Future<void> key({required int usage, required bool down, required int modifiers}) async =>
      keys.add([usage, down]);
}

Object? sessionWith(String resolution) => contractState('session', (state) {
      final session = (state['session'] as Map).cast<String, Object?>();
      final stats = (session['stats'] as Map).cast<String, Object?>();
      stats['resolution'] = resolution;
    });

void main() {
  testWidgets('waits for the picture, then shows the texture', (tester) async {
    final stream = RecordingStream();
    await pumpApp(tester, sessionWith('1920x1080'), stream: stream);
    expect(find.text('Waiting for video.'), findsOneWidget);
    expect(find.byType(Texture), findsNothing);

    stream.texture = 7;
    await tester.pump(const Duration(milliseconds: 600));
    await tester.pump();
    expect(find.text('Waiting for video.'), findsNothing);
    final texture = tester.widget<Texture>(find.byType(Texture));
    expect(texture.textureId, 7);
  });

  testWidgets('reports where the picture sits, in physical pixels, at the stream aspect',
      (tester) async {
    final stream = RecordingStream()..texture = 3;
    await pumpApp(tester, sessionWith('1600x900'), stream: stream);
    await tester.pump(const Duration(milliseconds: 600));
    await tester.pump();
    await tester.pump();
    expect(stream.rects, isNotEmpty);
    final rect = stream.rects.last;
    // The window is 1000x720 at ratio 1: a 16:9 picture fills the width, centred vertically.
    expect(rect[2], closeTo(1000, 0.5));
    expect(rect[3], closeTo(562.5, 0.5));
    expect(rect[1], closeTo((720 - 562.5) / 2, 0.5));
    expect(rect[4], 1);
  });

  testWidgets('Windows sends no pointer or key events from Dart', (tester) async {
    final stream = RecordingStream()..texture = 3;
    await pumpApp(tester, sessionWith('1600x900'), stream: stream);
    await tester.pump(const Duration(milliseconds: 600));
    await tester.pump();
    final gesture = await tester.startGesture(const Offset(500, 360));
    await gesture.up();
    await tester.sendKeyEvent(LogicalKeyboardKey.keyA);
    expect(stream.pointers, isEmpty);
    expect(stream.keys, isEmpty);
  });

  testWidgets('Android forwards touch relative to the video area', (tester) async {
    final stream = RecordingStream()..texture = 3;
    await pumpApp(
      tester,
      sessionWith('1600x900'),
      stream: stream,
      screen: Screen.phone,
      windows: false,
    );
    await tester.pump(const Duration(milliseconds: 600));
    await tester.pump();
    // The video box is 390 wide and 219.4 high, centred in 780 high.
    final top = (780 - 390 * 9 / 16) / 2;
    final gesture = await tester.startGesture(Offset(100, top + 50));
    await gesture.moveTo(Offset(120, top + 60));
    await gesture.up();
    expect(stream.pointers.map((p) => p.kind), ['down', 'move', 'up']);
    final down = stream.pointers.first;
    expect(down.x, closeTo(100, 0.5));
    expect(down.y, closeTo(50, 0.5));
    expect(down.areaWidth, closeTo(390, 0.5));
    expect(down.areaHeight, closeTo(219.4, 0.5));
  });

  testWidgets('Android forwards physical keys, Escape still opens the strip', (tester) async {
    final stream = RecordingStream()..texture = 3;
    await pumpApp(tester, sessionWith('1600x900'), stream: stream, windows: false);
    await tester.sendKeyDownEvent(LogicalKeyboardKey.keyA);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.keyA);
    expect(stream.keys, [
      [PhysicalKeyboardKey.keyA.usbHidUsage, true],
      [PhysicalKeyboardKey.keyA.usbHidUsage, false],
    ]);
    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    await tester.pump();
    expect(stream.keys.length, 2);
  });
}
