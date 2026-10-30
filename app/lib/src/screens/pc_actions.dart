import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../app.dart';
import '../model/model.dart';

StatusPill pcStatusPill(RemotePc pc) {
  switch (pc.problem) {
    case PcProblem.relayUnreachable:
      return const StatusPill('Relay unreachable', kind: StatusKind.attention);
    case PcProblem.notAccepted:
      return const StatusPill('Not accepted', kind: StatusKind.danger);
    case PcProblem.keyChanged:
      return const StatusPill('PC key changed', kind: StatusKind.danger);
    case null:
      return pc.online
          ? const StatusPill('Online', kind: StatusKind.online)
          : const StatusPill('Offline', kind: StatusKind.offline);
  }
}

/// "Last mode Game" for a PC that has been used, "Last seen 2 Oct" otherwise, "Never connected".
String pcLastText(RemotePc pc) {
  final mode = pc.lastMode;
  if (mode != null) return 'Last mode ${mode == StreamMode.game ? 'Game' : 'Work'}';
  final seen = pc.lastSeen;
  return seen == null ? 'Never connected' : 'Last seen $seen';
}

String _lastSession(RemotePc pc) {
  final seen = pc.lastSeen;
  final mode = pc.lastMode;
  final parts = [
    ?seen,
    if (mode != null) mode == StreamMode.game ? 'Game mode' : 'Work mode',
  ];
  return parts.isEmpty ? 'Never' : parts.join(', ');
}

/// Details of the selected PC on desktop, with Connect, Rename and Forget.
class PcDetail extends StatelessWidget {
  const PcDetail({super.key, required this.pc});

  final RemotePc pc;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    return Padding(
      padding: const EdgeInsets.all(24),
      child: DetailPanel(
        eyebrow: 'Selected',
        title: pc.name,
        id: pc.id,
        rows: [
          KeyValueRow('Relay', pc.relay, mono: true),
          KeyValueRow('Allowed', pc.allowed),
          KeyValueRow('Last session', _lastSession(pc)),
          if (pc.problem != null) ...[
            const SizedBox(height: 12),
            _ProblemNotice(pc.problem!),
          ],
        ],
        actions: [
          PrimaryButton(
            label: 'Connect to ${pc.name}',
            icon: Ico.monitorPlay,
            expand: true,
            onTap: () => controller.act(Connect(pcId: pc.id)),
          ),
          Row(
            children: [
              TextBtn('Rename', icon: Ico.pencil, onTap: () => renamePc(context, pc)),
              const Spacer(),
              TextBtn(
                'Forget ${pc.name}',
                icon: Ico.trash,
                onTap: () => forgetPc(context, pc),
              ),
            ],
          ),
        ],
      ),
    );
  }
}

class _ProblemNotice extends StatelessWidget {
  const _ProblemNotice(this.problem);

  final PcProblem problem;

  @override
  Widget build(BuildContext context) {
    final (title, message) = switch (problem) {
      PcProblem.relayUnreachable => (
          'Relay unreachable',
          "The PC's relay did not answer, so its state is unknown.",
        ),
      PcProblem.notAccepted => (
          'This device is not accepted',
          "The PC refused the last connection: this device's key is not on its list.",
        ),
      PcProblem.keyChanged => (
          'PC key changed',
          'The PC answered with a key that does not match the one saved when it was paired.',
        ),
    };
    return NoticeBanner(
      kind: problem == PcProblem.relayUnreachable ? NoticeKind.attention : NoticeKind.danger,
      title: title,
      message: message,
    );
  }
}

/// Phone: a sheet with Connect, Rename and Forget for one PC.
Future<void> showPcActions(BuildContext context, RemotePc pc) {
  final controller = AppScope.of(context).controller;
  return showSheet<void>(
    context,
    (sheet) => SheetSurface(
      children: [
        SheetHeader(pc.name),
        SheetItem(
          label: 'Connect',
          icon: Ico.monitorPlay,
          onTap: () {
            Navigator.of(sheet).pop();
            controller.act(Connect(pcId: pc.id));
          },
        ),
        SheetItem(
          label: 'Rename',
          icon: Ico.pencil,
          onTap: () {
            Navigator.of(sheet).pop();
            renamePc(context, pc);
          },
        ),
        SheetItem(
          label: 'Forget',
          icon: Ico.trash,
          danger: true,
          onTap: () {
            Navigator.of(sheet).pop();
            forgetPc(context, pc);
          },
        ),
      ],
    ),
  );
}

Future<void> forgetPc(BuildContext context, RemotePc pc) async {
  final controller = AppScope.of(context).controller;
  final yes = await showConfirm(
    context,
    title: 'Forget ${pc.name}?',
    message: 'This device will no longer be paired with it. Pair again to reconnect.',
    confirmLabel: 'Forget',
    danger: true,
  );
  if (yes) await controller.act(Forget(pcId: pc.id));
}

Future<void> renamePc(BuildContext context, RemotePc pc) async {
  final controller = AppScope.of(context).controller;
  final name = await showModalPanel<String>(
    context,
    (dialog) => _RenameBox(initial: pc.name),
    width: 420,
    shrink: true,
  );
  if (name != null && name.trim().isNotEmpty && name.trim() != pc.name) {
    await controller.act(Rename(pcId: pc.id, name: name.trim()));
  }
}

class _RenameBox extends StatefulWidget {
  const _RenameBox({required this.initial});

  final String initial;

  @override
  State<_RenameBox> createState() => _RenameBoxState();
}

class _RenameBoxState extends State<_RenameBox> {
  late final TextEditingController _controller = TextEditingController(text: widget.initial);

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.all(20),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text('Rename PC', style: ts(context.type.h3, context.colors.textPrimary)),
            const SizedBox(height: 12),
            SettingsField(
              controller: _controller,
              autofocus: true,
              onSubmitted: (text) => Navigator.of(context).pop(text),
            ),
            const SizedBox(height: 16),
            Row(
              mainAxisAlignment: MainAxisAlignment.end,
              children: [
                SecondaryButton(label: 'Cancel', onTap: () => Navigator.of(context).pop()),
                const SizedBox(width: 8),
                PrimaryButton(
                  label: 'Rename',
                  onTap: () => Navigator.of(context).pop(_controller.text),
                ),
              ],
            ),
          ],
        ),
      );
}
