import 'dart:io';

import 'package:flutter/widgets.dart';

import 'src/app.dart';
import 'src/app_controller.dart';
import 'src/backend/channel.dart';
import 'src/backend/frb_channel.dart';
import 'src/backend/pigeon_channel.dart';
import 'src/platform/key_files.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final isWindows = Platform.isWindows;
  final AppChannel channel = isWindows ? await FrbChannel.open() : PigeonChannel();
  final controller = AppController(channel);
  await controller.refresh();
  runApp(
    BubblePlayApp(
      env: AppEnv(
        controller: controller,
        isWindows: isWindows,
        keyFiles: const SystemKeyFiles(),
        theme: ValueNotifier<Brightness?>(null),
      ),
    ),
  );
}
