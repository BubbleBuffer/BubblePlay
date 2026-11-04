import 'dart:async';

import 'package:flutter/widgets.dart';

import '../session/monitor_previews.dart';

/// The snapshot of the monitor the session just switched to, shown over the video until the
/// first live frame of the new media generation arrives, then faded out.
///
/// When [selected] changes to a monitor of which the source holds a picture, the picture appears
/// at once, scaled up to fit the way the video does. It fades out ([fadeDuration]) as soon as the
/// source's live generation differs from the one at the switch. If that never happens (the host
/// switched before the change reached the screen) it fades after [maxWait], so a stale picture
/// never stays. Nothing is shown when no picture is held, and the overlay never takes input.
class MonitorPreviewOverlay extends StatefulWidget {
  const MonitorPreviewOverlay({
    super.key,
    required this.source,
    required this.selected,
    this.fadeDuration = const Duration(milliseconds: 200),
    this.pollInterval = const Duration(milliseconds: 50),
    this.maxWait = const Duration(seconds: 3),
  });

  final MonitorPreviewSource source;

  /// The selector of the monitor the session shows now; null while unknown.
  final String? selected;

  final Duration fadeDuration;

  /// How often the live generation is read while a picture is up.
  final Duration pollInterval;

  /// The longest a picture stays before it fades without a new generation.
  final Duration maxWait;

  @override
  State<MonitorPreviewOverlay> createState() => _MonitorPreviewOverlayState();
}

class _MonitorPreviewOverlayState extends State<MonitorPreviewOverlay>
    with SingleTickerProviderStateMixin {
  late final AnimationController _fade = AnimationController(vsync: this)
    ..addStatusListener(_fadeStatus);
  MonitorPreview? _preview;
  int _baseline = 0;
  Timer? _poll;
  Timer? _limit;

  @override
  void didUpdateWidget(MonitorPreviewOverlay old) {
    super.didUpdateWidget(old);
    final selected = widget.selected;
    if (selected == null || selected == old.selected) return;
    _switchedTo(selected);
  }

  @override
  void dispose() {
    _stopWatching();
    _fade.dispose();
    super.dispose();
  }

  void _switchedTo(String selector) {
    final preview = widget.source.take(selector);
    if (preview == null) {
      // Nothing held for the new monitor: let any picture of the old one go.
      if (_preview != null) _fadeOut();
      return;
    }
    _baseline = widget.source.liveGeneration;
    _fade.value = 1;
    setState(() => _preview = preview);
    _stopWatching();
    _poll = Timer.periodic(widget.pollInterval, (_) {
      if (widget.source.liveGeneration != _baseline) _fadeOut();
    });
    _limit = Timer(widget.maxWait, _fadeOut);
  }

  void _stopWatching() {
    _poll?.cancel();
    _limit?.cancel();
    _poll = null;
    _limit = null;
  }

  void _fadeOut() {
    _stopWatching();
    if (_preview == null || !mounted) return;
    unawaited(_fade.reverse(from: _fade.value == 0 ? 1 : _fade.value));
  }

  void _fadeStatus(AnimationStatus status) {
    if (status == AnimationStatus.dismissed && _poll == null && mounted) {
      setState(() => _preview = null);
    }
  }

  @override
  Widget build(BuildContext context) {
    _fade.duration = widget.fadeDuration;
    final preview = _preview;
    if (preview == null) return const SizedBox.shrink();
    final aspect = preview.width > 0 && preview.height > 0
        ? preview.width / preview.height
        : null;
    Widget picture = Image.memory(
      preview.jpeg,
      fit: BoxFit.fill,
      gaplessPlayback: true,
      filterQuality: FilterQuality.medium,
      errorBuilder: (_, __, ___) => const SizedBox.shrink(),
    );
    picture = aspect == null
        ? SizedBox.expand(child: picture)
        : AspectRatio(aspectRatio: aspect, child: picture);
    return IgnorePointer(
      child: FadeTransition(opacity: _fade, child: Center(child: picture)),
    );
  }
}
