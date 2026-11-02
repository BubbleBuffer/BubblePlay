import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import 'app_controller.dart';
import 'devices/devices_controller.dart';
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
class BubblePlayApp extends StatelessWidget {
  const BubblePlayApp({super.key, required this.env, this.brightness});

  final AppEnv env;

  /// Pins the brightness (tests and goldens); null uses the person's choice, then the system's.
  final Brightness? brightness;

  @override
  Widget build(BuildContext context) {
    final choice = env.theme;
    return ListenableBuilder(
      listenable: choice ?? const AlwaysStoppedAnimation<double>(0),
      builder: (context, _) => WidgetsApp(
        title: 'BubblePlay',
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
                      child: ScrollConfiguration(behavior: const AppScrollBehavior(), child: child!),
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
