import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../../app.dart';
import '../../model/model.dart';

/// The pairing wait: the number to compare with the PC, and how the request ended.
class PairConfirmView extends StatelessWidget {
  const PairConfirmView({super.key, required this.confirm});

  final PairConfirm confirm;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    void close() => controller.act(const CancelFlow());
    final c = context.colors;
    final waiting = confirm.outcome is Waiting;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      mainAxisSize: MainAxisSize.min,
      children: [
        KeyValueRow('PC', confirm.pcName),
        const Gap(Space.xxs),
        KeyValueRow('PC ID', confirm.pcId, mono: true),
        const Gap(Space.lg),
        ComparisonNumber(
          confirm.number.toString().padLeft(6, '0'),
          group: 3,
          label: 'Number',
        ),
        const Gap(Space.lg),
        _outcome(context, close),
        if (waiting) ...[
          const Gap(Space.sm),
          Text(
            'The same number must show on the PC.',
            style: ts(context.type.uiSmall, c.textTertiary, weight: Weight.regular),
          ),
        ],
      ],
    );
  }

  Widget _outcome(BuildContext context, VoidCallback close) => switch (confirm.outcome) {
        Waiting() => _waiting(context, close),
        final Approved approved => _answer(context, close, approved.by, approved: true),
        final Denied denied => _answer(context, close, denied.by, approved: false),
        TimedOut() => _banner(
            context,
            close,
            kind: NoticeKind.attention,
            title: 'Pairing timed out',
            message: 'Nobody answered before the request expired.',
            label: 'Close',
            primary: false,
          ),
        Cancelled() => _banner(
            context,
            close,
            kind: NoticeKind.attention,
            title: 'Pairing closed',
            message: 'The pairing window on the PC was closed.',
            label: 'Close',
            primary: false,
          ),
      };

  Widget _waiting(BuildContext context, VoidCallback close) {
    final c = context.colors;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      mainAxisSize: MainAxisSize.min,
      children: [
        Row(
          children: [
            const Spinner(),
            const Gap(Space.md),
            Expanded(
              child: Text(
                'Waiting for the PC to confirm',
                style: ts(context.type.ui, c.textPrimary),
              ),
            ),
            Text(
              '${confirm.waitingSecs} s',
              style: ts(context.type.uiSmall, c.textSecondary),
            ),
          ],
        ),
        const Gap(Space.xl),
        SecondaryButton(label: 'Cancel', onTap: close),
      ],
    );
  }

  Widget _answer(BuildContext context, VoidCallback close, Answerer by, {required bool approved}) {
    final who = switch (by) {
      Pc() => 'at the PC',
      Device(:final name) => 'by $name',
    };
    final verb = approved ? 'Approved' : 'Denied';
    return _banner(
      context,
      close,
      kind: approved ? NoticeKind.success : NoticeKind.danger,
      title: approved ? 'Paired' : 'Pairing denied',
      message: '$verb $who.',
      label: approved ? 'Done' : 'Close',
      primary: approved,
    );
  }

  Widget _banner(
    BuildContext context,
    VoidCallback close, {
    required NoticeKind kind,
    required String title,
    required String message,
    required String label,
    required bool primary,
  }) =>
      Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        mainAxisSize: MainAxisSize.min,
        children: [
          NoticeBanner(kind: kind, title: title, message: message),
          const Gap(Space.xl),
          if (primary)
            PrimaryButton(label: label, expand: true, onTap: close)
          else
            SecondaryButton(label: label, onTap: close),
        ],
      );
}
