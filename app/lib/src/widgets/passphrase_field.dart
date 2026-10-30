import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

/// A single-line field that hides what is typed, for key passphrases.
///
/// bb-ui has no obscured field yet (candidate for a bb-ui addition); this is the same
/// EditableText setup as its PlainField with hiding on and every learning feature off: no
/// suggestions, no autocorrect, no clipboard toolbar copy of the hidden text.
class PassphraseField extends StatefulWidget {
  const PassphraseField({
    super.key,
    required this.controller,
    this.placeholder = 'Passphrase',
    this.autofocus = false,
    this.onSubmitted,
  });

  final TextEditingController controller;
  final String placeholder;
  final bool autofocus;
  final ValueChanged<String>? onSubmitted;

  @override
  State<PassphraseField> createState() => _PassphraseFieldState();
}

class _PassphraseFieldState extends State<PassphraseField> {
  final FocusNode _focus = FocusNode();

  @override
  void initState() {
    super.initState();
    widget.controller.addListener(_changed);
  }

  @override
  void dispose() {
    widget.controller.removeListener(_changed);
    _focus.dispose();
    super.dispose();
  }

  void _changed() => setState(() {});

  @override
  Widget build(BuildContext context) {
    final c = context.colors;
    final style = ts(context.type.body, c.textPrimary);
    return FieldFrame(
      height: context.mobile ? 44 : 36,
      padding: const EdgeInsets.symmetric(horizontal: 10),
      child: Stack(
        alignment: Alignment.centerLeft,
        children: [
          if (widget.controller.text.isEmpty)
            IgnorePointer(
              child: Text(
                widget.placeholder,
                maxLines: 1,
                style: style.copyWith(color: c.textTertiary),
              ),
            ),
          EditableText(
            controller: widget.controller,
            focusNode: _focus,
            style: style,
            cursorColor: c.textPrimary,
            backgroundCursorColor: c.textTertiary,
            selectionColor: c.textSelection,
            cursorWidth: 1.5,
            autofocus: widget.autofocus,
            obscureText: true,
            autocorrect: false,
            enableSuggestions: false,
            enableInteractiveSelection: false,
            keyboardType: TextInputType.visiblePassword,
            textInputAction: TextInputAction.done,
            onSubmitted: widget.onSubmitted,
            maxLines: 1,
          ),
        ],
      ),
    );
  }
}
