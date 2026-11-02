// Opens the "Report a problem" sheet and stores what it sends in the outbox.
//
// Entry points: Ctrl+Shift+F anywhere in the app ([FeedbackHost]), the Settings row, and
// [showFeedbackSheet] for any other menu (the session chrome menu calls it). The screenshot is
// taken before the sheet covers the screen, from the Flutter UI only: never while a session is
// showing (the remote picture is a texture, and it is nobody's to keep), and a failed or slow
// capture just means no screenshot. Android's shake entry is not here: it needs the
// accelerometer plugin (see the lane report).

import 'dart:async';
import 'dart:io';
import 'dart:math' as math;
import 'dart:ui' as ui;

import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../app.dart';
import '../logging/app_log.dart';
import '../model/model.dart';
import '../screens/settings_screen.dart' show appVersion;
import 'feedback_sheet.dart';
import 'outbox.dart';
import 'report.dart';

/// Set at build time with `--dart-define=GIT_COMMIT=...`; empty when absent.
const _gitCommit = String.fromEnvironment('GIT_COMMIT');

/// Set at build time with `--dart-define=BUILD_NUMBER=...`.
const _buildNumber = String.fromEnvironment('BUILD_NUMBER', defaultValue: '1');

/// What the feedback flow needs from outside: where reports go, and (tests only) a stand-in
/// for the screenshot.
class FeedbackServices {
  const FeedbackServices({required this.outbox, this.capture, this.log});

  final Outbox outbox;

  /// Replaces the real screenshot (widget tests: the real one needs the GPU).
  final Future<Uint8List?> Function()? capture;

  /// The logger reports are built from; null uses [AppLog.instance].
  final AppLogger? log;
}

/// Tracks the name of the top route, for the report's `route`.
class RouteTracker extends NavigatorObserver {
  final List<Route<dynamic>> _stack = [];

  /// The top route's name, or null when it has none (the home page, a panel).
  String? get current {
    if (_stack.isEmpty) return null;
    final name = _stack.last.settings.name;
    return name == null || name == '/' ? null : name;
  }

  @override
  void didPush(Route<dynamic> route, Route<dynamic>? previousRoute) => _stack.add(route);

  @override
  void didPop(Route<dynamic> route, Route<dynamic>? previousRoute) => _stack.remove(route);

  @override
  void didRemove(Route<dynamic> route, Route<dynamic>? previousRoute) => _stack.remove(route);

  @override
  void didReplace({Route<dynamic>? newRoute, Route<dynamic>? oldRoute}) {
    final i = oldRoute == null ? -1 : _stack.indexOf(oldRoute);
    if (i >= 0) _stack.removeAt(i);
    if (newRoute != null) _stack.insert(i >= 0 ? i : _stack.length, newRoute);
  }
}

/// The screen the snapshot is showing, as a plain name.
String screenName(AppState? state) {
  if (state == null) return 'start';
  if (state.session != null) return 'session';
  final flow = state.flow;
  if (flow is Enroll) return 'enroll';
  if (flow != null) return 'pairing';
  return 'home';
}

class FeedbackController {
  FeedbackController({required this.captureKey, required this.tracker, this.services});

  /// Wraps the whole app; the screenshot is taken from it.
  final GlobalKey captureKey;
  final RouteTracker tracker;
  final FeedbackServices? services;

  bool _isOpen = false;

  /// True while the sheet is up; a second open does nothing meanwhile.
  bool get isOpen => _isOpen;

  AppLogger get _log => services?.log ?? AppLog.instance;

  /// Shows the sheet over the navigator [context] belongs to. Completes with true when a
  /// report was stored.
  Future<bool> open(BuildContext context) async {
    if (_isOpen) return false;
    _isOpen = true;
    try {
      final env = AppScope.of(context);
      final state = env.controller.state;
      final media = MediaQuery.of(context);
      final session = state?.session;
      final screenshot = session == null ? await _capture(media.devicePixelRatio) : null;
      final pc = session?.pcId;
      final info = _Info(
        deviceId: state?.thisDevice.id ?? '',
        pc: pc,
        route: tracker.current ?? screenName(state),
        locale: ui.PlatformDispatcher.instance.locale.toLanguageTag(),
      );
      if (!context.mounted) return false;
      final stored = await _showSheet<bool>(
        context,
        (sheet) => FeedbackSheet(
          screenshot: screenshot,
          onSend: (text, kind, shot) => _store(text, kind, shot, info),
          onStored: (sheetContext) => showToast(
            sheetContext,
            builder: (context, progress) => const ToastCard(
              title: 'Saved. It is sent to your PC the next time you connect.',
            ),
          ),
        ),
      );
      return stored == true;
    } catch (e, stack) {
      AppLog.error('Feedback', 'Could not open the report sheet', error: e, stack: stack);
      return false;
    } finally {
      _isOpen = false;
    }
  }

  Future<T?> _showSheet<T>(BuildContext context, WidgetBuilder builder) => context.mobile
      ? showSheet<T>(context, builder)
      : showModalPanel<T>(context, builder, width: 560, shrink: true);

  /// The Flutter UI as PNG, or null when there is no picture to take or it takes too long.
  Future<Uint8List?> _capture(double devicePixelRatio) async {
    final custom = services?.capture;
    try {
      if (custom != null) return await custom();
      final box = captureKey.currentContext?.findRenderObject();
      if (box is! RenderRepaintBoundary) return null;
      final image = await box
          .toImage(pixelRatio: math.min(devicePixelRatio, 2.0))
          .timeout(const Duration(seconds: 3));
      try {
        final data = await image.toByteData(format: ui.ImageByteFormat.png);
        return data?.buffer.asUint8List();
      } finally {
        image.dispose();
      }
    } catch (e) {
      AppLog.warn('Feedback', 'No screenshot', error: e);
      return null;
    }
  }

  /// Builds the report and queues it. False when it could not be stored.
  Future<bool> _store(String text, FeedbackKind kind, Uint8List? screenshot, _Info info) async {
    final outbox = services?.outbox;
    if (outbox == null) {
      AppLog.error('Feedback', 'No outbox to store the report in');
      return false;
    }
    try {
      final log = _log;
      final now = DateTime.now().toUtc();
      final id = newUuid();
      final logs = await log.reportLogs(pc: info.pc);
      final report = buildReport(
        log: log,
        now: now,
        id: id,
        text: text,
        kind: kind,
        app: const FeedbackApp(version: appVersion, build: _buildNumber, commit: _gitCommit),
        platform: Platform.operatingSystem,
        device: FeedbackDevice(id: info.deviceId, name: _hostName(), model: _osVersion()),
        pc: info.pc,
        session: null,
        route: info.route,
        locale: info.locale,
        logs: logs,
        screenshotFile: screenshot == null ? null : Outbox.screenshotName(id, now),
      );
      await outbox.add(report, screenshot: screenshot);
      AppLog.info('Feedback', 'Report stored', fields: {'kind': kind.name, 'id': id});
      return true;
    } catch (e, stack) {
      AppLog.error('Feedback', 'Could not store the report', error: e, stack: stack);
      return false;
    }
  }
}

class _Info {
  const _Info({
    required this.deviceId,
    required this.pc,
    required this.route,
    required this.locale,
  });

  final String deviceId;
  final String? pc;
  final String route;
  final String locale;
}

String _hostName() {
  try {
    return Platform.localHostname;
  } catch (_) {
    return 'unknown';
  }
}

String _osVersion() {
  try {
    return Platform.operatingSystemVersion;
  } catch (_) {
    return 'unknown';
  }
}

/// Opens the report sheet. Menus call this (the session chrome menu does); it does nothing
/// when no [FeedbackHost] is above [context]. Completes with true when a report was stored.
Future<bool> showFeedbackSheet(BuildContext context) {
  final scope = context.getInheritedWidgetOfExactType<_FeedbackScope>();
  if (scope == null) return Future.value(false);
  return scope.controller.open(context);
}

/// Gives the app its feedback controller, the screenshot boundary and Ctrl+Shift+F. Sits
/// above the navigator, inside [AppScope].
class FeedbackHost extends StatefulWidget {
  const FeedbackHost({
    super.key,
    required this.navigatorKey,
    required this.tracker,
    required this.child,
  });

  /// The navigator the sheet opens over (the shortcut has no context of its own).
  final GlobalKey<NavigatorState> navigatorKey;
  final RouteTracker tracker;
  final Widget child;

  @override
  State<FeedbackHost> createState() => _FeedbackHostState();
}

class _FeedbackHostState extends State<FeedbackHost> {
  final _captureKey = GlobalKey();
  FeedbackController? _controller;

  @override
  void initState() {
    super.initState();
    HardwareKeyboard.instance.addHandler(_onKey);
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    _controller ??= FeedbackController(
      captureKey: _captureKey,
      tracker: widget.tracker,
      services: AppScope.of(context).feedback,
    );
  }

  @override
  void dispose() {
    HardwareKeyboard.instance.removeHandler(_onKey);
    super.dispose();
  }

  bool _onKey(KeyEvent event) {
    if (event is! KeyDownEvent || event.logicalKey != LogicalKeyboardKey.keyF) return false;
    final keyboard = HardwareKeyboard.instance;
    if (!keyboard.isControlPressed || !keyboard.isShiftPressed) return false;
    if (keyboard.isAltPressed || keyboard.isMetaPressed) return false;
    final context = widget.navigatorKey.currentContext;
    final controller = _controller;
    if (context != null && controller != null && !controller.isOpen) {
      unawaited(controller.open(context));
    }
    return true;
  }

  @override
  Widget build(BuildContext context) => _FeedbackScope(
        controller: _controller!,
        child: RepaintBoundary(key: _captureKey, child: widget.child),
      );
}

class _FeedbackScope extends InheritedWidget {
  const _FeedbackScope({required this.controller, required super.child});

  final FeedbackController controller;

  @override
  bool updateShouldNotify(_FeedbackScope old) => old.controller != controller;
}
