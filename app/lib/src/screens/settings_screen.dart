import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../app.dart';
import '../feedback/feedback_controller.dart';
import '../logging/app_log.dart';
import '../model/model.dart';

/// The app version shown in About.
const appVersion = '0.1.0';

/// Settings: this device's key (ID, where it lives, export) and the theme choice. Desktop gets a
/// modal panel; a phone gets a full page. Both show the same body.
Future<void> openSettings(BuildContext context, AppState state) {
  final device = state.thisDevice;
  final body = _SettingsBody(deviceId: device.id, keyPath: device.keyPath);
  if (context.mobile) {
    return pushScreen<void>(
      context,
      (page) => ScreenScaffold(title: 'Settings', body: body),
      name: 'settings',
    );
  }
  return showModalPanel<void>(
    context,
    (panel) => _SettingsPanel(body: body),
    width: 560,
    height: 520,
  );
}

/// The desktop panel: title, close button, and the body.
class _SettingsPanel extends StatelessWidget {
  const _SettingsPanel({required this.body});

  final Widget body;

  @override
  Widget build(BuildContext context) => Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(24, 24, 24, 4),
            child: Row(
              children: [
                Expanded(
                  child: Text('Settings', style: ts(context.type.h3, context.colors.textPrimary)),
                ),
                IconBtn(
                  Ico.close,
                  semanticLabel: 'Close',
                  onTap: () => Navigator.of(context).pop(),
                ),
              ],
            ),
          ),
          Expanded(child: body),
        ],
      );
}

class _SettingsBody extends StatefulWidget {
  const _SettingsBody({required this.deviceId, required this.keyPath});

  final String deviceId;
  final String keyPath;

  @override
  State<_SettingsBody> createState() => _SettingsBodyState();
}

class _SettingsBodyState extends State<_SettingsBody> {
  /// Set when the key could not be written out; shown under the export row.
  bool _exportFailed = false;

  Future<void> _export() async {
    final env = AppScope.of(context);
    setState(() => _exportFailed = false);
    String text;
    try {
      text = await env.controller.exportKey();
    } catch (e) {
      AppLog.error('Settings', 'Key export failed', error: e);
      if (mounted) setState(() => _exportFailed = true);
      return;
    }
    if (!mounted) return;
    bool saved;
    try {
      saved = await env.keyFiles.save('bubbleplay-device-key.bpkey', text);
    } catch (e) {
      AppLog.error('Settings', 'Key file could not be saved', error: e);
      saved = false;
    }
    if (!mounted || !saved) return;
    showToast(
      context,
      builder: (context, progress) => const ToastCard(title: 'Key file saved'),
    );
  }

  @override
  Widget build(BuildContext context) {
    final env = AppScope.of(context);
    final mobile = context.mobile;
    final theme = env.theme;
    return ListView(
      padding: mobile ? const EdgeInsets.all(16) : const EdgeInsets.all(24),
      children: [
        const SettingsHeading('This device'),
        const SizedBox(height: 8),
        SettingsRow(
          label: 'Device ID',
          stack: mobile,
          trailing: SizedBox(
            width: mobile ? null : 240,
            child: CopyableText(widget.deviceId),
          ),
        ),
        SettingsRow(label: 'Key file', description: widget.keyPath),
        SettingsRow(
          label: 'Export key',
          description: 'Save a copy of the key to restore this device later.',
          trailing: OutlineButton(label: 'Export', onTap: _export),
        ),
        if (_exportFailed) ...[
          const SizedBox(height: 12),
          const NoticeBanner(
            kind: NoticeKind.danger,
            message: 'The key could not be exported.',
          ),
        ],
        const SettingsHeading('Appearance', top: 24),
        const SizedBox(height: 8),
        if (theme != null)
          SettingsRow(
            label: 'Theme',
            trailing: ValueListenableBuilder<Brightness?>(
              valueListenable: theme,
              builder: (context, value, _) => SelectButton<String>(
                value: _themeName(value),
                options: const ['System', 'Light', 'Dark'],
                labelOf: (name) => name,
                onChanged: (name) => theme.value = _themeBrightness(name),
              ),
            ),
          ),
        const SettingsHeading('About', top: 24),
        const SizedBox(height: 8),
        SettingsRow(
          label: 'Version',
          trailing: Text(appVersion, style: ts(context.type.ui, context.colors.textSecondary)),
        ),
        const SettingsRow(
          label: 'Stored on this device',
          description: 'The key and the list of PCs never leave this device.',
        ),
        const SettingsHeading('Support', top: 24),
        const SizedBox(height: 8),
        SettingsRow(
          label: 'Report a problem',
          description: 'Send a note with the recent logs from this device.',
          trailing: OutlineButton(
            label: 'Report',
            onTap: () => showFeedbackSheet(context),
          ),
        ),
      ],
    );
  }
}

String _themeName(Brightness? value) => switch (value) {
      null => 'System',
      Brightness.light => 'Light',
      Brightness.dark => 'Dark',
    };

Brightness? _themeBrightness(String name) => switch (name) {
      'Light' => Brightness.light,
      'Dark' => Brightness.dark,
      _ => null,
    };
