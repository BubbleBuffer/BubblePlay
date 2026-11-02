import 'package:bb_ui/bb_ui.dart';
import 'dart:async';

import 'package:flutter/widgets.dart' hide Action, Flow;

import 'app_controller.dart';
import 'devices/devices_controller.dart';
import 'feedback/feedback_controller.dart';
import 'feedback/shake_detector.dart';
import 'platform/key_files.dart';
import 'session/stream_channel.dart';
import 'screens/gate.dart';

/// What every screen needs besides the snapshot: the controller to act through, the platform
/// (Windows has a "This PC" section, Android does not) and the key file dialogs.
class AppEnv {
  const AppEnv({
    required this.controller,
    required this.isWindows,
    required this.keyFiles,
    this.theme,
    this.stream = const NoStreamChannel(),
    this.devices,
    this.initialSection,
    this.feedback,
    this.shakeSamples,
  });

  final AppController controller;
  final bool isWindows;
  final KeyFiles keyFiles;

  /// The native session video and input (see session/stream_channel.dart).
  final StreamChannel stream;

  /// The person's light/dark choice; null follows the system.
  final ValueNotifier<Brightness?>? theme;

  /// The host's Devices pipe (Windows only); null where there is no "This PC" to manage.
  final DevicesController? devices;

  /// The section to open on start (`--section=this-pc`); null opens the PC list's first PC.
  final String? initialSection;

  /// Where "Report a problem" stores reports; null makes Send report that it could not save.
  final FeedbackServices? feedback;

  /// Opens the accelerometer for shake-to-report; null (everywhere but Android) turns it off.
  final Stream<ShakeSample> Function()? shakeSamples;
}

class AppScope extends InheritedWidget {
  const AppScope({super.key, required this.env, required super.child});

  final AppEnv env;

  static AppEnv of(BuildContext context) {
    final scope = context.dependOnInheritedWidgetOfExactType<AppScope>();
    assert(scope != null, 'No AppScope above this widget');
    return scope!.env;
  }

  @override
  bool updateShouldNotify(AppScope oldWidget) => env != oldWidget.env;
}

/// The app root: bb-ui theme over a plain WidgetsApp (no Material).
class BubblePlayApp extends StatefulWidget {
  const BubblePlayApp({super.key, required this.env, this.brightness});

  final AppEnv env;

  /// Pins the brightness (tests and goldens); null uses the person's choice, then the system's.
  final Brightness? brightness;

  @override
  State<BubblePlayApp> createState() => _BubblePlayAppState();
}

class _BubblePlayAppState extends State<BubblePlayApp> {
  final _navigatorKey = GlobalKey<NavigatorState>();
  final _tracker = RouteTracker();
  StreamSubscription<void>? _feedbackRequests;
  ShakeService? _shake;

  @override
  void initState() {
    super.initState();
    if (widget.env.shakeSamples != null) {
      _shake = ShakeService(
        samples: widget.env.shakeSamples,
        onShake: _onShake,
        allowed: _shakeAllowed,
      );
    }
    // The tray's "Report a problem" item arrives as `openFeedback` on the devices pipe.
    final devices = widget.env.devices;
    if (devices == null) return;
    _feedbackRequests = devices.feedbackRequests.listen((_) => _openFeedback(devices));
    if (devices.feedbackPending) {
      // It arrived while the app was starting: open once the first frame has a navigator.
      WidgetsBinding.instance.addPostFrameCallback((_) => _openFeedback(devices));
    }
  }

  @override
  void dispose() {
    _shake?.dispose();
    _feedbackRequests?.cancel();
    super.dispose();
  }

  /// Never while a remote session is on screen: a shake there is the phone moving with the game.
  bool _shakeAllowed() =>
      screenName(widget.env.controller.state) != 'session' && !(_tracker.current ?? '').contains('session');

  void _onShake() {
    final context = _navigatorKey.currentContext;
    if (!mounted || context == null) return;
    unawaited(showFeedbackSheet(context));
  }

  void _openFeedback(DevicesController devices) {
    final context = _navigatorKey.currentContext;
    if (!mounted || context == null) return;
    devices.consumeFeedback();
    unawaited(showFeedbackSheet(context));
  }

  @override
  Widget build(BuildContext context) {
    final env = widget.env;
    final brightness = widget.brightness;
    final choice = env.theme;
    return ListenableBuilder(
      listenable: choice ?? const AlwaysStoppedAnimation<double>(0),
      builder: (context, _) => WidgetsApp(
        title: 'BubblePlay',
        navigatorKey: _navigatorKey,
        navigatorObservers: [_tracker],
        color: AppColors.light.desk,
        debugShowCheckedModeBanner: false,
        pageRouteBuilder: <T>(settings, builder) => PageRouteBuilder<T>(
          settings: settings,
          pageBuilder: (context, animation, secondary) => builder(context),
        ),
        builder: (context, child) {
          final dark = (brightness ?? choice?.value ?? MediaQuery.platformBrightnessOf(context)) ==
              Brightness.dark;
          return LayoutBuilder(
            builder: (context, constraints) => AppTheme(
              colors: dark ? AppColors.darkTheme : AppColors.light,
              formFactor: FormFactor.forWidth(constraints.maxWidth),
              child: AppScope(
                env: env,
                child: Builder(
                  builder: (context) => ColoredBox(
                    color: context.colors.bgPrimary,
                    child: DefaultTextStyle(
                      style: ts(context.type.body, context.colors.textPrimary),
                      child: ScrollConfiguration(
                        behavior: const AppScrollBehavior(),
                        child: FeedbackHost(
                          navigatorKey: _navigatorKey,
                          tracker: _tracker,
                          child: child!,
                        ),
                      ),
                    ),
                  ),
                ),
              ),
            ),
          );
        },
        home: const Gate(),
      ),
    );
  }
}
