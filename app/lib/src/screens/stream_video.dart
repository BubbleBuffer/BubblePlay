import 'dart:async';

import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/widgets.dart';

import '../session/stream_channel.dart';

/// The session picture: the platform's native video as a Flutter `Texture`, centred at the
/// stream's aspect ratio, with the input Android needs forwarded from here.
///
/// Until the platform has a picture it shows [waiting]. The video itself never passes through
/// Dart; this widget only asks for the texture id, reports where the picture sits (Windows
/// captures the keyboard and mouse on the window and needs the rectangle), and on Android sends
/// touch to the platform.
class StreamVideo extends StatefulWidget {
  const StreamVideo({
    super.key,
    required this.stream,
    required this.resolution,
    required this.forwardPointer,
    required this.waiting,
    this.pollInterval = const Duration(milliseconds: 500),
  });

  final StreamChannel stream;

  /// The stream size as the model reports it, for example `1920x1080`; empty until known.
  final String resolution;

  /// Send pointer events to the platform (Android); Windows reads them natively.
  final bool forwardPointer;

  /// What to show until there is a picture.
  final String waiting;

  final Duration pollInterval;

  @override
  State<StreamVideo> createState() => _StreamVideoState();
}

class _StreamVideoState extends State<StreamVideo> {
  final GlobalKey _box = GlobalKey();
  int? _texture;
  Timer? _poll;
  Rect? _reported;
  bool _frameQueued = false;
  double _ratio = 1;

  @override
  void initState() {
    super.initState();
    _ask();
    _poll = Timer.periodic(widget.pollInterval, (_) => _ask());
  }

  @override
  void dispose() {
    _poll?.cancel();
    // The picture is gone: let the platform stop clipping and mapping for it.
    unawaited(widget.stream.setVideoRect(0, 0, 0, 0, _ratio).catchError((_) {}));
    super.dispose();
  }

  Future<void> _ask() async {
    int? id;
    try {
      id = await widget.stream.textureId();
    } catch (_) {
      id = null;
    }
    if (!mounted || id == _texture) return;
    setState(() => _texture = id);
  }

  /// Aspect ratio of the stream, or null while unknown.
  double? get _aspect {
    final match = RegExp(r'(\d+)\D+(\d+)').firstMatch(widget.resolution);
    if (match == null) return null;
    final w = int.parse(match.group(1)!);
    final h = int.parse(match.group(2)!);
    return w > 0 && h > 0 ? w / h : null;
  }

  void _reportRect() {
    _frameQueued = false;
    if (!mounted) return;
    final box = _box.currentContext?.findRenderObject();
    if (box is! RenderBox || !box.hasSize) return;
    final ratio = MediaQuery.devicePixelRatioOf(context);
    final origin = box.localToGlobal(Offset.zero) * ratio;
    final rect = origin & (box.size * ratio);
    if (rect == _reported) return;
    _reported = rect;
    _ratio = ratio;
    unawaited(
      widget.stream
          .setVideoRect(rect.left, rect.top, rect.width, rect.height, ratio)
          .catchError((_) {}),
    );
  }

  void _pointer(String kind, PointerEvent event, Size area, {Offset scroll = Offset.zero}) {
    unawaited(
      widget.stream
          .pointer(
            StreamPointer(
              kind: kind,
              id: event.pointer,
              x: event.localPosition.dx,
              y: event.localPosition.dy,
              areaWidth: area.width,
              areaHeight: area.height,
              buttons: event.buttons,
              dx: scroll.dx,
              dy: scroll.dy,
              timeMs: event.timeStamp.inMilliseconds,
            ),
          )
          .catchError((_) {}),
    );
  }

  @override
  Widget build(BuildContext context) {
    if (!_frameQueued) {
      _frameQueued = true;
      WidgetsBinding.instance.addPostFrameCallback((_) => _reportRect());
    }
    final texture = _texture;
    if (texture == null) {
      return Center(
        child: Padding(
          padding: const EdgeInsets.all(24),
          child: Text(
            widget.waiting,
            textAlign: TextAlign.center,
            style: ts(context.type.ui, AppColors.darkTheme.textSecondary),
          ),
        ),
      );
    }
    Widget video = Texture(textureId: texture, filterQuality: FilterQuality.medium);
    if (widget.forwardPointer) {
      video = LayoutBuilder(
        builder: (context, constraints) {
          final area = constraints.biggest;
          return Listener(
            behavior: HitTestBehavior.opaque,
            onPointerDown: (e) => _pointer('down', e, area),
            onPointerMove: (e) => _pointer('move', e, area),
            onPointerUp: (e) => _pointer('up', e, area),
            onPointerCancel: (e) => _pointer('cancel', e, area),
            onPointerSignal: (e) {
              if (e is PointerScrollEvent) {
                _pointer('scroll', e, area, scroll: e.scrollDelta);
              }
            },
            child: video,
          );
        },
      );
    }
    final aspect = _aspect;
    return Center(
      child: aspect == null
          ? SizedBox.expand(key: _box, child: video)
          : AspectRatio(key: _box, aspectRatio: aspect, child: video),
    );
  }
}
