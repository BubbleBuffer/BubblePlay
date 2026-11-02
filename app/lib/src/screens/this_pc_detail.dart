import 'dart:async';

import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;
import 'package:qr/qr.dart';

import '../devices/devices_controller.dart';
import '../devices/devices_model.dart';

/// The detail pane for "This PC": the host's name and relay state, its paired devices, and the
/// pairing flow. Everything comes from the host through [DevicesController]; with the host out
/// of reach it says so and offers no actions.
class ThisPcDetail extends StatelessWidget {
  const ThisPcDetail({super.key, required this.devices});

  final DevicesController devices;

  @override
  Widget build(BuildContext context) => ListenableBuilder(
        listenable: devices,
        builder: (context, _) {
          final state = devices.state;
          return ListView(
            padding: const EdgeInsets.all(24),
            children: [
              _Header(state: state),
              const SizedBox(height: 20),
              if (state.connection case LinkDisconnected(:final reason))
                _NotConnected(reason: reason)
              else if (state.connection is LinkConnecting)
                const _Connecting()
              else ...[
                if (state.saveError case final message?) ...[
                  NoticeBanner(
                    kind: NoticeKind.danger,
                    title: 'Could not save',
                    message: message,
                    onDismiss: devices.dismiss,
                  ),
                  const SizedBox(height: 16),
                ],
                _PairingSection(devices: devices, state: state),
                const SizedBox(height: 28),
                _DeviceList(devices: devices, state: state),
              ],
            ],
          );
        },
      );
}

class _Header extends StatelessWidget {
  const _Header({required this.state});

  final DevicesState state;

  @override
  Widget build(BuildContext context) {
    final c = context.colors;
    final connected = state.isConnected;
    final pcId = state.pcId;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const SectionLabel('This PC', icon: Ico.server),
        const SizedBox(height: 8),
        Wrap(
          spacing: 10,
          runSpacing: 6,
          crossAxisAlignment: WrapCrossAlignment.center,
          children: [
            Text(
              state.pcName ?? 'This PC',
              style: tsDisplay(context.type.h1, c.textPrimary, weight: 700),
            ),
            if (connected)
              StatusPill(
                state.relayOnline ? 'Relay online' : 'Relay offline',
                kind: state.relayOnline ? StatusKind.online : StatusKind.attention,
              )
            else
              const StatusPill('Not connected', kind: StatusKind.offline),
          ],
        ),
        if (pcId != null) ...[
          const SizedBox(height: 12),
          Row(
            children: [
              Text('PC ID', style: ts(context.type.uiSmall, c.textSecondary)),
              const SizedBox(width: 8),
              Flexible(child: CopyableText(pcId, size: 13)),
            ],
          ),
        ],
      ],
    );
  }
}

class _Connecting extends StatelessWidget {
  const _Connecting();

  @override
  Widget build(BuildContext context) => Row(
        children: [
          const Spinner(),
          const SizedBox(width: 10),
          Text(
            'Connecting to the BubblePlay host',
            style: ts(context.type.ui, context.colors.textSecondary),
          ),
        ],
      );
}

/// The host cannot be reached. Pairing and revoking are not offered.
class _NotConnected extends StatelessWidget {
  const _NotConnected({required this.reason});

  final DisconnectReason reason;

  @override
  Widget build(BuildContext context) {
    final (kind, title, message) = switch (reason) {
      DisconnectReason.hostNotRunning => (
          NoticeKind.attention,
          'BubblePlay host is not running',
          'Start the BubblePlay host on this PC to pair and manage devices. '
              'This page connects by itself once it runs.',
        ),
      DisconnectReason.notTrusted => (
          NoticeKind.danger,
          'Host not trusted',
          'The program answering is not run by this Windows user, so the app did not connect. '
              'Restart BubblePlay while signed in as the user running this app.',
        ),
      DisconnectReason.protocol => (
          NoticeKind.attention,
          'Host sent something unreadable',
          'The connection to the BubblePlay host was closed. Retrying. '
              'If this keeps happening, update the app and the host to the same version.',
        ),
      DisconnectReason.io => (
          NoticeKind.attention,
          'Connection to the host lost',
          'The link to the BubblePlay host broke. Retrying. Paired devices are not affected.',
        ),
      DisconnectReason.unsupported => (
          NoticeKind.danger,
          'Host version not supported',
          'This app and the BubblePlay host use different versions of the devices connection. '
              'Update both to the same version.',
        ),
    };
    return NoticeBanner(kind: kind, title: title, message: message);
  }
}

class _PairingSection extends StatelessWidget {
  const _PairingSection({required this.devices, required this.state});

  final DevicesController devices;
  final DevicesState state;

  @override
  Widget build(BuildContext context) {
    final c = context.colors;
    final ty = context.type;
    return switch (state.pairing) {
      PairingNone() => PrimaryButton(
          label: 'Pair a device',
          icon: Ico.plus,
          expand: true,
          onTap: devices.startPairing,
        ),
      final PairingWaiting waiting => Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text('Pair a device', style: tsDisplay(ty.h3, c.textPrimary, weight: 700)),
            const SizedBox(height: 4),
            Text(
              'On the other device, add this PC with the PC ID and the code, or scan the QR code.',
              style: ts(ty.ui, c.textSecondary, weight: 400),
            ),
            const SizedBox(height: 16),
            OneTimeCode(
              waiting.codeDisplay,
              groups: const [5, 5],
              label: 'One-time code',
              caption: 'PC ID ${waiting.pcId}',
            ),
            const SizedBox(height: 16),
            Center(
              child: QrFrame(
                size: 200,
                caption: 'Scan with BubblePlay on the other device',
                child: CustomPaint(painter: _QrPainter(waiting.qrUri, QrFrame.ink)),
              ),
            ),
            const SizedBox(height: 16),
            _Countdown(devices: devices),
            const SizedBox(height: 12),
            SecondaryButton(label: 'Cancel pairing', onTap: devices.cancelPairing),
          ],
        ),
      final PairingRequest request => Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text('${request.name} wants to pair',
                style: tsDisplay(ty.h3, c.textPrimary, weight: 700)),
            const SizedBox(height: 12),
            ComparisonNumber(
              request.sasDisplay,
              label: 'Comparison number',
              caption: 'Allow only if ${request.name} shows the same number.',
            ),
            const SizedBox(height: 16),
            // Deny comes first (and takes focus first): the safe answer is the one nearest
            // to hand, Allow is the dominant action.
            Row(
              children: [
                Expanded(
                  child: SecondaryButton(
                    label: 'Deny',
                    danger: true,
                    onTap: () => devices.deny(request.id),
                  ),
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: PrimaryButton(
                    label: 'Allow',
                    icon: Ico.check,
                    onTap: () => devices.allow(request.id),
                  ),
                ),
              ],
            ),
          ],
        ),
      PairingDone(:final name) => _Ended(
          devices: devices,
          kind: NoticeKind.success,
          title: 'Paired with $name',
          message: '$name can now connect to this PC. It is in the list below.',
          primaryLabel: 'Done',
          closeOnly: true,
        ),
      PairingDenied() => _Ended(
          devices: devices,
          kind: NoticeKind.info,
          title: 'Pairing denied',
          message: 'You denied the request. The device was not added.',
        ),
      PairingFailed() => _Ended(
          devices: devices,
          kind: NoticeKind.danger,
          title: 'Pairing failed',
          message: 'Pairing did not complete and no device was added. Start again for a new code.',
        ),
      PairingExpired() => _Ended(
          devices: devices,
          kind: NoticeKind.attention,
          title: 'Code expired',
          message: 'No device paired in time. Start again for a new code.',
        ),
      PairingStopped() => _Ended(
          devices: devices,
          kind: NoticeKind.info,
          title: 'Pairing stopped',
          message: 'Pairing was stopped on this PC. No device was added.',
        ),
    };
  }
}

/// How an ended pairing reads and what to do next: pair again, or close the message.
class _Ended extends StatelessWidget {
  const _Ended({
    required this.devices,
    required this.kind,
    required this.title,
    required this.message,
    this.primaryLabel = 'Pair a device',
    this.closeOnly = false,
  });

  final DevicesController devices;
  final NoticeKind kind;
  final String title;
  final String message;
  final String primaryLabel;
  final bool closeOnly;

  @override
  Widget build(BuildContext context) => Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          NoticeBanner(kind: kind, title: title, message: message),
          const SizedBox(height: 12),
          if (closeOnly)
            PrimaryButton(label: primaryLabel, expand: true, onTap: devices.dismiss)
          else ...[
            PrimaryButton(
              label: primaryLabel,
              icon: Ico.plus,
              expand: true,
              onTap: devices.startPairing,
            ),
            const SizedBox(height: 8),
            SecondaryButton(label: 'Close', onTap: devices.dismiss),
          ],
        ],
      );
}

/// "Expires in 4:32", redrawn each second from the controller's clock.
class _Countdown extends StatefulWidget {
  const _Countdown({required this.devices});

  final DevicesController devices;

  @override
  State<_Countdown> createState() => _CountdownState();
}

class _CountdownState extends State<_Countdown> {
  Timer? _timer;

  @override
  void initState() {
    super.initState();
    _timer = Timer.periodic(const Duration(seconds: 1), (_) => setState(() {}));
  }

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final left = widget.devices.remaining ?? Duration.zero;
    final minutes = left.inMinutes;
    final seconds = (left.inSeconds % 60).toString().padLeft(2, '0');
    return Row(
      mainAxisAlignment: MainAxisAlignment.center,
      children: [
        Icon20(Ico.clock, size: 16, color: context.colors.iconSecondary),
        const SizedBox(width: 6),
        Text(
          left == Duration.zero ? 'Code expired' : 'Expires in $minutes:$seconds',
          style: ts(context.type.ui, context.colors.textSecondary),
        ),
      ],
    );
  }
}

/// Draws a QR code's dark modules in [ink] over the frame's paper.
class _QrPainter extends CustomPainter {
  _QrPainter(this.data, this.ink);

  final String data;
  final Color ink;

  @override
  void paint(Canvas canvas, Size size) {
    final image = QrImage(QrCode.fromData(data: data, errorCorrectLevel: QrErrorCorrectLevel.M));
    final count = image.moduleCount;
    final cell = size.shortestSide / count;
    final paint = Paint()..color = ink;
    for (var row = 0; row < count; row++) {
      for (var col = 0; col < count; col++) {
        if (image.isDark(row, col)) {
          canvas.drawRect(
            Rect.fromLTWH(col * cell, row * cell, cell + .5, cell + .5),
            paint,
          );
        }
      }
    }
  }

  @override
  bool shouldRepaint(_QrPainter old) => old.data != data || old.ink != ink;
}

class _DeviceList extends StatelessWidget {
  const _DeviceList({required this.devices, required this.state});

  final DevicesController devices;
  final DevicesState state;

  @override
  Widget build(BuildContext context) {
    final c = context.colors;
    final list = state.devices;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        SectionHeader('Paired devices', large: false),
        const SizedBox(height: 8),
        if (list.isEmpty)
          Text(
            'No devices are paired with this PC yet.',
            style: ts(context.type.ui, c.textSecondary, weight: 400),
          )
        else
          for (final device in list)
            DeviceRow(
              key: ValueKey(device.id),
              icon: Ico.userCheck,
              title: device.name,
              status: device.connected
                  ? const StatusPill('Connected', kind: StatusKind.online)
                  : null,
              meta: [if (!device.connected) lastSeenText(device.lastSeen, devices.now)],
              trailing: SecondaryButton(
                label: 'Revoke',
                danger: true,
                height: 32,
                onTap: () => _revoke(context, device),
              ),
            ),
      ],
    );
  }

  Future<void> _revoke(BuildContext context, PairedDevice device) async {
    final ok = await showConfirm(
      context,
      title: 'Revoke ${device.name}?',
      message: '${device.name} can no longer connect to this PC. '
          'It can pair again with a new code.',
      confirmLabel: 'Revoke ${device.name}',
      danger: true,
    );
    if (ok) devices.revoke(device.id);
  }
}

/// "Last seen 5 min ago", "Last seen 12 Oct", or "Never seen".
String lastSeenText(DateTime? seen, DateTime now) {
  if (seen == null) return 'Never seen';
  final ago = now.difference(seen);
  if (ago.inMinutes < 1) return 'Last seen just now';
  if (ago.inMinutes < 60) return 'Last seen ${ago.inMinutes} min ago';
  if (ago.inHours < 24) return 'Last seen ${ago.inHours} h ago';
  if (ago.inDays < 7) return 'Last seen ${ago.inDays} d ago';
  const months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  final local = seen.toLocal();
  return 'Last seen ${local.day} ${months[local.month - 1]}';
}
