import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../app.dart';
import '../model/model.dart';
import '../widgets/passphrase_field.dart';

/// The file this device stores its own key in. Its passphrase prompt is the unlock step, not an
/// import from another device.
const String ownKeyFileName = 'device-key.bpkey';

/// The name an exported key file is saved under.
const String exportedKeyFileName = 'bubbleplay-device-key.bpkey';

/// First run and key unlock: create a key, import a key file, enter a passphrase.
class EnrollScreen extends StatelessWidget {
  const EnrollScreen({super.key, required this.state, required this.step});

  final AppState state;
  final EnrollStep step;

  @override
  Widget build(BuildContext context) {
    final thisDevice = state.thisDevice;
    return switch (step) {
      Choose() => const _Choose(),
      Created() => _Created(deviceId: thisDevice.id, keyPath: thisDevice.keyPath),
      ImportUnlock(:final fileName, :final error) =>
        _Unlock(fileName: fileName, error: error),
      ImportRefused(:final fileName, :final reason) =>
        _Refused(fileName: fileName, reason: reason),
    };
  }
}

/// Reads a key file the person picks and hands its text to the core. Nothing is sent when the
/// person cancels the dialog.
Future<void> importKeyFile(BuildContext context) async {
  final env = AppScope.of(context);
  final file = await env.keyFiles.pick();
  if (file == null) return;
  await env.controller.act(ImportKeyText(fileName: file.name, text: file.text));
}

/// The centred column every step sits in: 440 px on desktop, the full width with 16 px sides on a
/// phone.
class _StepPage extends StatelessWidget {
  const _StepPage({required this.children});

  final List<Widget> children;

  @override
  Widget build(BuildContext context) {
    final mobile = context.mobile;
    final page = Center(
      child: SingleChildScrollView(
        padding: EdgeInsets.symmetric(
          horizontal: mobile ? 16 : 0,
          vertical: mobile ? 24 : 32,
        ),
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 440),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: children,
          ),
        ),
      ),
    );
    return mobile ? SafeArea(child: page) : page;
  }
}

/// No key yet: Create key or import one from another device.
class _Choose extends StatelessWidget {
  const _Choose();

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    return _StepPage(
      children: [
        const SectionHeader(
          'Set up this device',
          subtitle:
              'BubblePlay identifies this device with a key. Create one here or '
              'import a key file from another device.',
        ),
        const SizedBox(height: 24),
        PrimaryButton(
          label: 'Create key',
          onTap: () => controller.act(const CreateKey()),
        ),
        const SizedBox(height: 8),
        SecondaryButton(
          label: 'Import key file',
          onTap: () => importKeyFile(context),
        ),
      ],
    );
  }
}

/// A new key was made: show its device ID, where it is stored, and how to keep a copy.
class _Created extends StatefulWidget {
  const _Created({required this.deviceId, required this.keyPath});

  final String deviceId;
  final String keyPath;

  @override
  State<_Created> createState() => _CreatedState();
}

class _CreatedState extends State<_Created> {
  /// Set once a key file was written, so the person knows the export happened.
  bool _saved = false;

  Future<void> _export() async {
    final env = AppScope.of(context);
    final text = await env.controller.exportKey();
    if (!mounted) return;
    final saved = await env.keyFiles.save(exportedKeyFileName, text);
    if (!mounted) return;
    if (saved) setState(() => _saved = true);
  }

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    final mobile = context.mobile;
    return _StepPage(
      children: [
        const SectionHeader(
          'Key created',
          subtitle: 'Export a copy of the key to restore this device later.',
        ),
        const SizedBox(height: 24),
        SectionLabel('Device ID'),
        const SizedBox(height: 8),
        FittedBox(
          fit: BoxFit.scaleDown,
          alignment: Alignment.centerLeft,
          child: CodeDisplay(
            widget.deviceId,
            groups: const [4, 4, 4, 4],
            size: mobile ? CodeDisplaySize.medium : CodeDisplaySize.large,
          ),
        ),
        const SizedBox(height: 20),
        const SectionLabel('Stored at', icon: Ico.folder),
        const SizedBox(height: 8),
        CopyableText(widget.keyPath, size: mobile ? 12 : 13),
        if (_saved) ...[
          const SizedBox(height: 20),
          const NoticeBanner(
            kind: NoticeKind.info,
            message: 'Key file saved.',
          ),
        ],
        const SizedBox(height: 24),
        PrimaryButton(
          label: 'Continue',
          onTap: () => controller.act(const CancelFlow()),
        ),
        const SizedBox(height: 8),
        SecondaryButton(
          label: 'Export key',
          icon: Ico.copy,
          onTap: _export,
        ),
      ],
    );
  }
}

/// A key file with a passphrase: the device's own stored key, or one imported from elsewhere.
class _Unlock extends StatefulWidget {
  const _Unlock({required this.fileName, this.error});

  final String fileName;
  final String? error;

  @override
  State<_Unlock> createState() => _UnlockState();
}

class _UnlockState extends State<_Unlock> {
  /// The only place the passphrase lives: it is never copied into state, a log or a Text.
  final TextEditingController _passphrase = TextEditingController();

  @override
  void initState() {
    super.initState();
    _passphrase.addListener(_changed);
  }

  @override
  void didUpdateWidget(_Unlock oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.fileName != widget.fileName) _passphrase.clear();
  }

  @override
  void dispose() {
    _passphrase.removeListener(_changed);
    _passphrase.dispose();
    super.dispose();
  }

  void _changed() => setState(() {});

  void _submit() {
    final text = _passphrase.text;
    if (text.isEmpty) return;
    final controller = AppScope.of(context).controller;
    _passphrase.clear();
    controller.act(UnlockImport(passphrase: text));
  }

  @override
  Widget build(BuildContext context) {
    final colors = context.colors;
    final controller = AppScope.of(context).controller;
    final own = widget.fileName == ownKeyFileName;
    final error = widget.error;
    return _StepPage(
      children: [
        SectionHeader(
          own ? 'Unlock your key' : 'Import key file',
          subtitle: own
              ? 'This device has a key. Enter its passphrase to use it.'
              : '${widget.fileName} is protected with a passphrase.',
        ),
        const SizedBox(height: 20),
        if (error != null) ...[
          NoticeBanner(kind: NoticeKind.danger, message: error),
          const SizedBox(height: 16),
        ],
        PassphraseField(
          controller: _passphrase,
          autofocus: true,
          onSubmitted: (_) => _submit(),
        ),
        const SizedBox(height: 8),
        Text(
          'The passphrase is only used on this device.',
          style: ts(context.type.uiSmall, colors.textTertiary),
        ),
        const SizedBox(height: 24),
        PrimaryButton(
          label: 'Unlock',
          onTap: _passphrase.text.isEmpty ? null : _submit,
        ),
        if (!own) ...[
          const SizedBox(height: 8),
          SecondaryButton(
            label: 'Cancel',
            onTap: () => controller.act(const CancelFlow()),
          ),
        ],
      ],
    );
  }
}

/// The file is not a usable key: say which file and why, and offer another one.
class _Refused extends StatelessWidget {
  const _Refused({required this.fileName, required this.reason});

  final String fileName;
  final String reason;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    return _StepPage(
      children: [
        const SectionHeader(
          'Key file not accepted',
          subtitle: 'Choose another file, or go back and create a key instead.',
        ),
        const SizedBox(height: 20),
        NoticeBanner(
          kind: NoticeKind.danger,
          title: fileName,
          message: reason,
        ),
        const SizedBox(height: 24),
        PrimaryButton(
          label: 'Choose another file',
          onTap: () => importKeyFile(context),
        ),
        const SizedBox(height: 8),
        SecondaryButton(
          label: 'Back',
          onTap: () => controller.act(const CancelFlow()),
        ),
      ],
    );
  }
}
