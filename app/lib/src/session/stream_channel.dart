import 'package:flutter/services.dart';

/// One pointer event over the video area, in logical pixels relative to the area's top-left.
/// `kind` is one of down, move, up, cancel, scroll. Scroll carries `dx`/`dy` in logical pixels.
class StreamPointer {
  const StreamPointer({
    required this.kind,
    required this.id,
    required this.x,
    required this.y,
    required this.areaWidth,
    required this.areaHeight,
    this.buttons = 0,
    this.dx = 0,
    this.dy = 0,
    this.timeMs = 0,
  });

  final String kind;
  final int id;
  final double x;
  final double y;
  final double areaWidth;
  final double areaHeight;
  final int buttons;
  final double dx;
  final double dy;
  final int timeMs;

  Map<String, Object?> toMap() => {
        'kind': kind,
        'id': id,
        'x': x,
        'y': y,
        'areaWidth': areaWidth,
        'areaHeight': areaHeight,
        'buttons': buttons,
        'dx': dx,
        'dy': dy,
        'timeMs': timeMs,
      };
}

/// The native video of the current session: the texture and the input that Dart has to forward.
///
/// Video never passes through Dart. The platform renders the decoded stream into a Flutter
/// texture; Dart only shows it with a `Texture` widget and reports where it sits.
/// Windows captures keyboard, mouse and Raw Input natively on the window, so there only
/// [textureId] and [setVideoRect] matter. Android forwards touch and keys from Dart.
abstract interface class StreamChannel {
  /// The Flutter texture id of the session video, or null while there is no picture yet.
  Future<int?> textureId();

  /// Where the video sits in the window, in physical pixels of the client area, and the
  /// device pixel ratio used to get there. Sent whenever the layout changes.
  Future<void> setVideoRect(double x, double y, double width, double height, double ratio);

  /// A pointer event over the video (Android; ignored on Windows).
  Future<void> pointer(StreamPointer event);

  /// A key event (Android; ignored on Windows). `usage` is the USB HID usage of the physical key.
  Future<void> key({required int usage, required bool down, required int modifiers});
}

/// The channel both platforms implement natively: `bubbleplay/stream`.
class MethodStreamChannel implements StreamChannel {
  MethodStreamChannel([MethodChannel? channel])
      : _channel = channel ?? const MethodChannel('bubbleplay/stream');

  final MethodChannel _channel;

  @override
  Future<int?> textureId() => _channel.invokeMethod<int>('textureId');

  @override
  Future<void> setVideoRect(double x, double y, double width, double height, double ratio) =>
      _channel.invokeMethod<void>('videoRect', {
        'x': x,
        'y': y,
        'width': width,
        'height': height,
        'ratio': ratio,
      });

  @override
  Future<void> pointer(StreamPointer event) =>
      _channel.invokeMethod<void>('pointer', event.toMap());

  @override
  Future<void> key({required int usage, required bool down, required int modifiers}) =>
      _channel.invokeMethod<void>('key', {'usage': usage, 'down': down, 'modifiers': modifiers});
}

/// A stand-in with no video, for tests and platforms without a stream.
class NoStreamChannel implements StreamChannel {
  const NoStreamChannel();
  @override
  Future<int?> textureId() async => null;
  @override
  Future<void> setVideoRect(double x, double y, double width, double height, double ratio) async {}
  @override
  Future<void> pointer(StreamPointer event) async {}
  @override
  Future<void> key({required int usage, required bool down, required int modifiers}) async {}
}
