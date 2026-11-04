import 'dart:async';

import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/services.dart' show KeyDownEvent, KeyUpEvent, LogicalKeyboardKey;
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../app.dart';
import '../model/model.dart';
import 'monitor_preview_overlay.dart';
import 'stream_video.dart';

/// The session: the native video (see [StreamVideo]) under the session chrome.
///
/// The picture is the video's colour, not a theme colour, so it stays black in both themes. The
/// chrome hangs from the top edge: the tab at rest, the strip once it is opened, then whatever the
/// session wants the person to know, then the stats panel.
class SessionScreen extends StatefulWidget {
  const SessionScreen({super.key, required this.state, required this.session});

  final AppState state;
  final Session session;

  @override
  State<SessionScreen> createState() => _SessionScreenState();
}

class _SessionScreenState extends State<SessionScreen> {
  final FocusNode _keys = FocusNode(debugLabel: 'session');

  /// The strip is open (the tab is its handle, so the tab stays above it).
  bool _open = false;

  /// The stats panel is open.
  bool _stats = false;

  Session get _session => widget.session;

  /// The selector of the monitor on screen, or null before the host's list.
  String? get _selectedSelector {
    final monitors = _session.monitors;
    if (monitors.isEmpty) return null;
    final index = _session.monitor;
    return monitors[index >= 0 && index < monitors.length ? index : 0].selector;
  }

  @override
  void dispose() {
    _keys.dispose();
    super.dispose();
  }

  void _act(Action action) => AppScope.of(context).controller.act(action);

  void _toggleStrip() => setState(() => _open = !_open);

  KeyEventResult _onKey(FocusNode node, KeyEvent event) {
    if (event is KeyDownEvent && event.logicalKey == LogicalKeyboardKey.escape) {
      _toggleStrip();
      return KeyEventResult.handled;
    }
    // Windows captures the keyboard natively; Android sends the physical key on.
    if (!AppScope.of(context).isWindows && event.logicalKey != LogicalKeyboardKey.escape) {
      final usage = event.physicalKey.usbHidUsage;
      unawaited(
        AppScope.of(context)
            .stream
            .key(usage: usage, down: event is! KeyUpEvent, modifiers: 0)
            .catchError((_) {}),
      );
      return KeyEventResult.handled;
    }
    return KeyEventResult.ignored;
  }

  Future<void> _pickDisplay() async {
    final monitors = _session.monitors;
    if (monitors.isEmpty) return;
    final selected = _session.monitor >= 0 && _session.monitor < monitors.length
        ? monitors[_session.monitor].selector
        : monitors.first.selector;
    await showPickerSheet<void>(
      context,
      (sheet) => DisplayPicker(
        displays: [
          for (final monitor in monitors)
            DisplayInfo(
              id: monitor.selector,
              label: monitor.name,
              width: monitor.width,
              height: monitor.height,
              x: monitor.x,
              y: monitor.y,
              detail: '${monitor.refreshHz} Hz',
            ),
        ],
        selectedId: selected,
        subtitle: '${monitors.length} on ${_session.pcName}',
        autofocus: true,
        onSelect: (display) {
          Navigator.of(sheet).pop();
          final index = monitors.indexWhere((m) => m.selector == display.id);
          if (index >= 0) _act(SetMonitor(index));
        },
      ),
      desktopHeight: 420,
    );
  }

  @override
  Widget build(BuildContext context) {
    final width = MediaQuery.sizeOf(context).width;
    final compact = context.mobile;

    return Focus(
      focusNode: _keys,
      autofocus: true,
      onKeyEvent: _onKey,
      child: ColoredBox(
        color: const Color(0xFF000000),
        child: Stack(
          children: [
            Positioned.fill(
              child: StreamVideo(
                stream: AppScope.of(context).stream,
                resolution: _session.stats.resolution,
                forwardPointer: !AppScope.of(context).isWindows,
                waiting: 'Waiting for video.',
              ),
            ),
            Positioned.fill(
              child: MonitorPreviewOverlay(
                source: AppScope.of(context).previews,
                selected: _selectedSelector,
              ),
            ),
            Positioned(
              top: 0,
              left: 0,
              right: 0,
              child: SafeArea(
                bottom: false,
                child: Column(
                  mainAxisSize: MainAxisSize.min,
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    Center(
                      child: EdgeTab(
                        label: _session.pcName,
                        value: _modeText(_session.mode),
                        mark: context.colors.accent,
                        opened: _open,
                        onTap: _toggleStrip,
                      ),
                    ),
                    if (_open) ...[
                      const SizedBox(height: 6),
                      Center(child: _strip(context)),
                    ],
                    for (final notice in _notices(context))
                      Padding(
                        padding: const EdgeInsets.fromLTRB(12, 8, 12, 0),
                        child: Center(
                          child: ConstrainedBox(
                            constraints: const BoxConstraints(maxWidth: 620),
                            child: notice,
                          ),
                        ),
                      ),
                    if (_stats)
                      Padding(
                        padding: const EdgeInsets.fromLTRB(12, 8, 12, 0),
                        child: Center(
                          child: MetricsPanel(
                            title: 'Stream',
                            width: compact ? width - 24 : 270,
                            metrics: _metrics(),
                            trend: _session.stats.frameMs,
                            trendCaption: 'Frame time',
                          ),
                        ),
                      ),
                  ],
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _strip(BuildContext context) {
    final controls = _session.qualityControlsAvailable;
    final audio = _session.audioControlsAvailable;
    final hasMonitors = _session.monitors.isNotEmpty;
    final strip = EdgeStrip(
      groups: [
        Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            for (final mode in StreamMode.values)
              EdgeStripButton(
                icon: mode == StreamMode.work ? Ico.pointer : Ico.gamepad,
                label: _modeText(mode),
                selected: _session.mode == mode,
                onTap: () => _act(SetMode(mode)),
              ),
          ],
        ),
        EdgeStripButton(
          icon: Ico.monitor,
          label: 'Display',
          menu: true,
          onTap: hasMonitors ? _pickDisplay : null,
        ),
        Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            for (final quality in Quality.values)
              EdgeStripButton(
                icon: Ico.gauge,
                label: quality.wire,
                selected: _session.quality == quality,
                onTap: controls ? () => _act(SetQuality(quality)) : null,
              ),
          ],
        ),
        EdgeStripButton(
          icon: Ico.volume,
          label: 'Audio',
          selected: _session.audio,
          onTap: audio ? () => _act(SetAudio(!_session.audio)) : null,
        ),
        EdgeStripButton(
          icon: Ico.activity,
          label: 'Stats',
          selected: _stats,
          onTap: () => setState(() => _stats = !_stats),
        ),
        EdgeStripButton(
          icon: Ico.logOut,
          label: 'Leave',
          onTap: () => _act(const Disconnect()),
        ),
      ],
    );
    if (context.mobile) {
      return SizedBox(
        width: double.infinity,
        child: SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          child: strip,
        ),
      );
    }
    return FittedBox(
      fit: BoxFit.scaleDown,
      alignment: Alignment.topCenter,
      child: strip,
    );
  }

  List<Widget> _notices(BuildContext context) {
    final notices = <Widget>[];
    final link = _session.link;
    if (link is Reconnecting) {
      notices.add(
        NoticeBanner(
          kind: NoticeKind.attention,
          message: 'Reconnecting, attempt ${link.attempt}',
        ),
      );
    }
    switch (_session.notice) {
      case SessionNotice.adminRequired:
        notices.add(
          const NoticeBanner(
            kind: NoticeKind.attention,
            message:
                'Input to an elevated window is blocked until Admin mode is on.',
          ),
        );
      case SessionNotice.secureDesktop:
        notices.add(
          const NoticeBanner(
            kind: NoticeKind.attention,
            message:
                'The PC is showing the sign-in screen or a permission prompt.',
          ),
        );
      case null:
        break;
    }
    return notices;
  }

  List<Metric> _metrics() {
    final stats = _session.stats;
    String text(String value) => value.isEmpty ? '-' : value;
    return [
      Metric('Resolution', text(stats.resolution)),
      Metric('Frame rate', '${_number(stats.fps)} fps'),
      Metric('Bitrate', '${_number(stats.bitrateMbps)} Mb/s'),
      Metric('Latency', '${stats.rttMs} ms'),
      Metric('Decode', '${_number(stats.decodeMs)} ms'),
      Metric('Dropped', '${stats.dropped}'),
      Metric('Transport', text(stats.transport)),
      Metric('Codec', text(stats.codec)),
    ];
  }
}

/// "Work" or "Game": what the stream is set up for.
String _modeText(StreamMode mode) => mode == StreamMode.game ? 'Game' : 'Work';

/// A measurement with at most one decimal, no trailing ".0".
String _number(double value) =>
    value == value.roundToDouble() ? value.toStringAsFixed(0) : value.toStringAsFixed(1);
