import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../app.dart';
import 'enroll_screen.dart' show exportedKeyFileName;

/// Asks how the key file is protected, then saves it where the person picks. The file is sealed
/// under a passphrase typed here; leaving it unprotected takes a second, explicit confirm.
/// Returns whether a file was written. Errors propagate to the caller.
Future<bool> exportKeyFile(BuildContext context) async {
  final env = AppScope.of(context);
  String? passphrase;
  if (env.controller.sealsKeyExport) {
    final choice = await showModalPanel<String>(
      context,
      (panel) => const _PassphraseBox(),
      width: Widths.form,
      shrink: true,
    );
    if (choice == null || !context.mounted) return false;
    if (choice.isEmpty) {
      final plain = await showConfirm(
        context,
        title: 'Export without passphrase?',
        message: 'Anyone with this file can use this device key.',
        confirmLabel: 'Export',
        danger: true,
      );
      if (!plain || !context.mounted) return false;
    } else {
      passphrase = choice;
    }
  }
  final text = await env.controller.exportKey(passphrase: passphrase);
  if (!context.mounted) return false;
  return env.keyFiles.save(exportedKeyFileName, text);
}

/// Pops the passphrase, or an empty string for "no passphrase"; cancel pops null.
class _PassphraseBox extends StatefulWidget {
  const _PassphraseBox();

  @override
  State<_PassphraseBox> createState() => _PassphraseBoxState();
}

class _PassphraseBoxState extends State<_PassphraseBox> {
  final TextEditingController _passphrase = TextEditingController();

  @override
  void dispose() {
    _passphrase.dispose();
    super.dispose();
  }

  void _submit() {
    if (_passphrase.text.isNotEmpty) Navigator.of(context).pop(_passphrase.text);
  }

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.all(Space.xl),
        child: ListenableBuilder(
          listenable: _passphrase,
          builder: (context, _) => Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Text('Key file passphrase', style: ts(context.type.h3, context.colors.textPrimary)),
              const Gap(Space.md),
              ObscuredField(
                controller: _passphrase,
                autofocus: true,
                onSubmitted: (_) => _submit(),
              ),
              const Gap(Space.lg),
              PrimaryButton(
                label: 'Export',
                onTap: _passphrase.text.isEmpty ? null : _submit,
              ),
              const Gap(Space.sm),
              SecondaryButton(
                label: 'Export without passphrase',
                onTap: () => Navigator.of(context).pop(''),
              ),
              const Gap(Space.sm),
              SecondaryButton(label: 'Cancel', onTap: () => Navigator.of(context).pop()),
            ],
          ),
        ),
      );
}
