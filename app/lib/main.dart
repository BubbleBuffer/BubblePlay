import 'dart:io';

import 'package:flutter/widgets.dart';

import 'src/app.dart';
import 'src/app_controller.dart';
import 'src/backend/channel.dart';
import 'src/backend/frb_channel.dart';
import 'src/backend/pigeon_channel.dart';
import 'src/devices/devices_controller.dart';
import 'src/devices/frb_devices_channel.dart';
import 'src/feedback/feedback_controller.dart';
import 'src/feedback/outbox.dart';
import 'src/feedback/shake_sensor.dart';
import 'src/logging/app_log.dart';
import 'src/logging/error_hooks.dart';
import 'src/logging/upload/ctl_bridge_factory.dart';
import 'src/logging/upload/upload_coordinator.dart';
import 'src/platform/key_files.dart';
import 'src/session/stream_channel.dart';
import 'src/presence/presence_lifecycle.dart';
import 'src/presence/presence_service.dart';
import 'src/screens/settings_screen.dart' show appVersion;

Future<void> main(List<String> args) async => runGuarded(() => _start(args));

Future<void> _start(List<String> args) async {
  WidgetsFlutterBinding.ensureInitialized();
  final logRoot = resolveLogRoot();
  try {
    await AppLog.open(root: logRoot);
  } catch (_) {
    // No log file is better than no app: the in-memory ring still works.
  }
  installErrorHooks();
  AppLog.info('main', 'App started', fields: {'version': appVersion, 'platform': Platform.operatingSystem});
  final isWindows = Platform.isWindows;
  final AppChannel channel = isWindows ? await FrbChannel.open() : PigeonChannel();
  final controller = AppController(channel, presence: PresenceService());
  PresenceLifecycle(controller.setForeground);
  await controller.refresh();
  final devicesChannel = isWindows ? FrbDevicesChannel.tryOpen() : null;
  const sectionFlag = '--section=';
  final section = args
      .where((arg) => arg.startsWith(sectionFlag))
      .map((arg) => arg.substring(sectionFlag.length))
      .firstOrNull;
  final outbox = Outbox.forRoot(logRoot);
  final bridge = createPlatformCtlBridge();
  final uploads = bridge == null
      ? null
      : UploadCoordinator(
          bridge: bridge,
          appLog: AppLog.instance,
          outbox: outbox,
          stream: AppLog.instance.src,
        );
  uploads?.start();
  runApp(
    BubblePlayApp(
      env: AppEnv(
        controller: controller,
        isWindows: isWindows,
        keyFiles: const SystemKeyFiles(),
        stream: MethodStreamChannel(),
        theme: ValueNotifier<Brightness?>(null),
        devices: devicesChannel == null ? null : DevicesController(devicesChannel),
        initialSection: section,
        feedback: FeedbackServices(
          outbox: outbox,
          sent: uploads?.sentReports,
          onQueued: uploads?.checkFeedback,
        ),
        shakeSamples: Platform.isAndroid ? accelerometerSamples : null,
      ),
    ),
  );
}
