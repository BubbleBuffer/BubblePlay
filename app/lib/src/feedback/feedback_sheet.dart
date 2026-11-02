// The "Report a problem" sheet: a kind, a text box ("What happened?"), the screenshot of this
// screen (removable) and Send / Cancel. A bottom sheet on a phone, a panel on desktop. Escape
// closes it. It pops with true once [onSend] has stored the report; Cancel stores nothing.


import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';

import 'report.dart';

class FeedbackSheet extends StatefulWidget {
  const FeedbackSheet({
    super.key,
    required this.screenshot,
    required this.onSend,
    this.onStored,
  });

  /// The PNG of this screen, or null when there is none to attach.
  final Uint8List? screenshot;

  /// Stores the report; completes with true when it is stored.
  final Future<bool> Function(String text, FeedbackKind kind, Uint8List? screenshot) onSend;

  /// Called with the sheet's context once the report is stored, just before the sheet closes
  /// (the place to show the state line: this context is under the overlay).
  final void Function(BuildContext context)? onStored;

  @override
  State<FeedbackSheet> createState() => _FeedbackSheetState();
}

class _FeedbackSheetState extends State<FeedbackSheet> {
  final _text = TextEditingController();
  final _focus = FocusNode();
  late Uint8List? _screenshot = widget.screenshot;
  FeedbackKind _kind = FeedbackKind.bug;
  bool _sending = false;
  bool _failed = false;

  @override
  void dispose() {
    _text.dispose();
    _focus.dispose();
    super.dispose();
  }

  Future<void> _send() async {
    if (_sending) return;
    setState(() {
      _sending = true;
      _failed = false;
    });
    var stored = false;
    try {
      stored = await widget.onSend(_text.text, _kind, _screenshot);
    } catch (_) {}
    if (!mounted) return;
    if (stored) {
      widget.onStored?.call(context);
      Navigator.of(context).pop(true);
    } else {
      // The text stays as it was, for another try.
      setState(() {
        _sending = false;
        _failed = true;
      });
    }
  }

  void _cancel() => Navigator.of(context).maybePop();

  @override
  Widget build(BuildContext context) {
    final c = context.colors;
    final ty = context.type;
    final mobile = context.mobile;
    final title = Semantics(
      header: true,
      child: Text('Report a problem', style: ts(ty.h3, c.textPrimary)),
    );
    final body = Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Row(
          children: [
            Expanded(child: Text('Kind', style: ts(ty.ui, c.textSecondary))),
            SelectButton<FeedbackKind>(
              key: const ValueKey('feedback-kind'),
              value: _kind,
              options: FeedbackKind.values,
              labelOf: (kind) => kind.label,
              title: 'Kind',
              onChanged: _sending ? null : (kind) => setState(() => _kind = kind),
            ),
          ],
        ),
        const SizedBox(height: 12),
        ExcludeSemantics(
          child: Text('What happened?', style: ts(ty.ui, c.textSecondary)),
        ),
        const SizedBox(height: 6),
        Semantics(
          label: 'What happened?',
          textField: true,
          multiline: true,
          child: ConstrainedBox(
            constraints: const BoxConstraints(minHeight: 96),
            child: FieldFrame(
              padding: const EdgeInsets.all(10),
              child: EditableText(
                key: const ValueKey('feedback-text'),
                controller: _text,
                focusNode: _focus,
                style: ts(ty.body, c.textPrimary),
                cursorColor: c.textPrimary,
                backgroundCursorColor: c.textTertiary,
                selectionColor: c.textSelection,
                cursorWidth: 1.5,
                autofocus: !mobile,
                keyboardType: TextInputType.multiline,
                textInputAction: TextInputAction.newline,
                selectionControls: noteSelectionControls,
                maxLines: null,
                minLines: 3,
              ),
            ),
          ),
        ),
        if (_screenshot case final shot?) ...[
          const SizedBox(height: 12),
          Align(
            alignment: Alignment.centerLeft,
            child: Container(
              padding: const EdgeInsets.all(1),
              decoration: BoxDecoration(
                border: Border.all(color: c.border),
                borderRadius: Radii.ctl,
              ),
              child: ClipRRect(
                borderRadius: Radii.ctl,
                child: ConstrainedBox(
                  constraints: BoxConstraints(maxHeight: mobile ? 140 : 120),
                  child: Image.memory(
                    shot,
                    key: const ValueKey('feedback-screenshot'),
                    fit: BoxFit.contain,
                    semanticLabel: 'Screenshot of this screen',
                    errorBuilder: (context, error, stack) => const SizedBox.shrink(),
                  ),
                ),
              ),
            ),
          ),
          Align(
            alignment: Alignment.centerLeft,
            child: TextBtn(
              'Remove screenshot',
              onTap: _sending ? null : () => setState(() => _screenshot = null),
            ),
          ),
        ],
        const SizedBox(height: 8),
        Text(
          'Includes the recent log lines and errors from this device.',
          style: ts(ty.uiSmall, c.textTertiary),
        ),
        if (_failed) ...[
          const SizedBox(height: 12),
          const NoticeBanner(
            kind: NoticeKind.danger,
            message: 'The report could not be saved on this device.',
          ),
        ],
      ],
    );
    final buttons = Row(
      children: [
        Expanded(
          child: SecondaryButton(
            key: const ValueKey('feedback-cancel'),
            label: 'Cancel',
            onTap: _cancel,
          ),
        ),
        const SizedBox(width: 8),
        Expanded(
          child: PrimaryButton(
            key: const ValueKey('feedback-send'),
            label: 'Send',
            onTap: _sending ? null : _send,
          ),
        ),
      ],
    );
    final Widget content;
    if (mobile) {
      final media = MediaQuery.of(context);
      final room = media.size.height * 0.85 - 134 - media.padding.bottom;
      content = SheetSurface(
        children: [
          Padding(padding: const EdgeInsets.symmetric(horizontal: 16), child: title),
          const SizedBox(height: 12),
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 16),
            child: ConstrainedBox(
              constraints: BoxConstraints(maxHeight: room < 120 ? 120 : room),
              child: SingleChildScrollView(child: body),
            ),
          ),
          const SizedBox(height: 16),
          Padding(padding: const EdgeInsets.fromLTRB(16, 0, 16, 8), child: buttons),
        ],
      );
    } else {
      content = Padding(
        padding: const EdgeInsets.all(20),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            title,
            const SizedBox(height: 12),
            Flexible(child: SingleChildScrollView(child: body)),
            const SizedBox(height: 16),
            buttons,
          ],
        ),
      );
    }
    return CallbackShortcuts(
      bindings: {const SingleActivator(LogicalKeyboardKey.escape): _cancel},
      child: content,
    );
  }
}
