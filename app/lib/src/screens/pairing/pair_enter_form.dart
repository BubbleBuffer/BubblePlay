import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../../app.dart';
import '../../model/model.dart';

const _pcIdGroups = [4, 4, 4, 4];
const _codeGroups = [5, 5];

/// The pairing input: the PC ID and one-time code from the tray, or a remote request that
/// needs no code.
class PairEnterForm extends StatefulWidget {
  const PairEnterForm({super.key, required this.remote, this.error});

  final bool remote;
  final String? error;

  @override
  State<PairEnterForm> createState() => _PairEnterFormState();
}

class _PairEnterFormState extends State<PairEnterForm> {
  final _link = TextEditingController();
  final _pcId = TextEditingController();
  final _code = TextEditingController();
  final _relay = TextEditingController();

  @override
  void dispose() {
    _link.dispose();
    _pcId.dispose();
    _code.dispose();
    _relay.dispose();
    super.dispose();
  }

  void _submit() {
    final controller = AppScope.of(context).controller;
    final pcId = formatCode(_pcId.text, _pcIdGroups);
    final relay = _relay.text.trim();
    if (widget.remote) {
      controller.act(RequestRemotePairing(pcId: pcId, relay: relay));
      return;
    }
    final link = _link.text.trim();
    controller.act(
      SubmitPairing(
        link: link.isEmpty ? null : link,
        pcId: pcId,
        code: formatCode(_code.text, _codeGroups),
        relay: relay,
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final c = context.colors;
    final controller = AppScope.of(context).controller;
    final error = widget.error;
    final withCode = !widget.remote;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      mainAxisSize: MainAxisSize.min,
      children: [
        Text(
          withCode
              ? 'On the PC, open Pair device in the tray. It shows the PC ID, a one-time code '
                  'and a link.'
              : 'Ask a PC that allows remote pairing. A device that is already paired can '
                  'approve it.',
          style: ts(context.type.ui, c.textSecondary),
        ),
        const SizedBox(height: 20),
        if (withCode) ...[
          const SectionLabel('Pairing link (optional)'),
          const SizedBox(height: 6),
          SettingsField(
            controller: _link,
            mono: true,
            placeholder: 'https://relay.example/pair#...',
          ),
          const SizedBox(height: 16),
        ],
        const SectionLabel('PC ID'),
        const SizedBox(height: 6),
        CodeField(
          controller: _pcId,
          groups: _pcIdGroups,
          placeholder: 'XXXX-XXXX-XXXX-XXXX',
          semanticLabel: 'PC ID',
        ),
        if (withCode) ...[
          const SizedBox(height: 16),
          const SectionLabel('One-time code'),
          const SizedBox(height: 6),
          CodeField(
            controller: _code,
            groups: _codeGroups,
            placeholder: 'XXXXX-XXXXX',
            semanticLabel: 'One-time code',
          ),
        ],
        const SizedBox(height: 16),
        const SectionLabel('Relay'),
        const SizedBox(height: 6),
        SettingsField(
          controller: _relay,
          mono: true,
          placeholder: 'https://relay.example',
          onSubmitted: (_) => _submit(),
        ),
        if (error != null) ...[
          const SizedBox(height: 16),
          NoticeBanner(
            kind: NoticeKind.danger,
            title: 'Pairing did not start',
            message: error,
          ),
        ],
        const SizedBox(height: 20),
        Row(
          children: [
            Expanded(
              child: PrimaryButton(
                label: withCode ? 'Pair' : 'Send request',
                onTap: _submit,
              ),
            ),
            const SizedBox(width: 8),
            SecondaryButton(label: 'Cancel', onTap: () => controller.act(const CancelFlow())),
          ],
        ),
        const SizedBox(height: 8),
        Align(
          alignment: Alignment.centerLeft,
          child: TextBtn(
            withCode ? 'Pair without a code' : 'Use a code instead',
            onTap: () =>
                controller.act(withCode ? const StartRemotePairing() : const StartPairing()),
          ),
        ),
      ],
    );
  }
}
