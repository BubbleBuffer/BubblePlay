import 'dart:async';

import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../app.dart';
import '../model/model.dart';
import '../devices/devices_controller.dart';
import 'pc_actions.dart';
import 'settings_screen.dart';
import 'this_pc_detail.dart';

/// Your PCs, with "This PC" pinned above them on Windows. Desktop shows the list beside the
/// selected PC's details; a phone shows the list alone and opens a PC's actions from its row.
class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key, required this.state});

  final AppState state;

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> {
  String? _selectedId;
  bool _thisPcSelected = false;
  StreamSubscription<void>? _shows;

  bool _started = false;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    if (_started) return;
    _started = true;
    final env = AppScope.of(context);
    final devices = env.devices;
    if (devices == null) return;
    // A `show` from the host (the tray's Devices item) or --section=this-pc picks This PC.
    _thisPcSelected = env.initialSection == 'this-pc' || devices.showPending;
    devices.consumeShow();
    _shows = devices.shows.listen((_) {
      devices.consumeShow();
      if (mounted) setState(() => _thisPcSelected = true);
    });
  }

  @override
  void dispose() {
    _shows?.cancel();
    super.dispose();
  }

  RemotePc? get _selected {
    final pcs = widget.state.pcs;
    for (final pc in pcs) {
      if (pc.id == _selectedId) return pc;
    }
    return pcs.isEmpty ? null : pcs.first;
  }

  @override
  Widget build(BuildContext context) {
    final env = AppScope.of(context);
    final mobile = context.mobile;
    final list = _PcList(
      state: widget.state,
      showThisPc: env.isWindows,
      devices: env.devices,
      thisPcSelected: _thisPcSelected,
      onSelectThisPc: () {
        final devices = env.devices;
        if (mobile && devices != null) {
          // Narrow window: the detail is a pushed page, like a PC's actions.
          pushScreen<void>(
            context,
            (_) => ScreenScaffold(title: 'This PC', body: ThisPcDetail(devices: devices)),
          );
          return;
        }
        setState(() => _thisPcSelected = true);
      },
      selectedId: _thisPcSelected ? null : _selected?.id,
      onSelect: (pc) => setState(() {
        _selectedId = pc.id;
        _thisPcSelected = false;
      }),
      mobile: mobile,
    );
    if (mobile) return SafeArea(child: list);
    final selected = _selected;
    return Row(
      children: [
        Expanded(child: list),
        const VLine(height: double.infinity),
        SizedBox(
          width: 380,
          child: _thisPcSelected && env.devices != null
              ? ThisPcDetail(devices: env.devices!)
              : selected == null
              ? const SizedBox.shrink()
              : PcDetail(key: ValueKey(selected.id), pc: selected),
        ),
      ],
    );
  }
}

class _PcList extends StatelessWidget {
  const _PcList({
    required this.state,
    required this.showThisPc,
    required this.devices,
    required this.thisPcSelected,
    required this.onSelectThisPc,
    required this.selectedId,
    required this.onSelect,
    required this.mobile,
  });

  final AppState state;
  final bool showThisPc;
  final DevicesController? devices;
  final bool thisPcSelected;
  final VoidCallback onSelectThisPc;
  final String? selectedId;
  final ValueChanged<RemotePc> onSelect;
  final bool mobile;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    final pcs = state.pcs;
    final pad = mobile ? 16.0 : 32.0;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Expanded(
          child: ListView(
            padding: EdgeInsets.fromLTRB(pad, mobile ? 16 : 24, pad, 16),
            children: [
              if (showThisPc) ...[
                const SectionLabel('This PC', icon: Ico.monitorPlay),
                const SizedBox(height: 8),
                ThisPcRow(
                  host: state.host,
                  devices: devices,
                  selected: thisPcSelected,
                  onSelect: onSelectThisPc,
                ),
                const SizedBox(height: 16),
                const HLine(),
                const SizedBox(height: 24),
              ],
              SectionHeader(
                'Your PCs',
                subtitle: "Other PCs paired with this device's key.",
                trailing: mobile
                    ? null
                    : IntrinsicWidth(
                        child: SecondaryButton(
                          label: 'Pair a new PC',
                          icon: Ico.plus,
                          height: 36,
                          onTap: () => controller.act(const StartPairing()),
                        ),
                      ),
              ),
              const SizedBox(height: 12),
              if (pcs.isEmpty)
                EmptyState(
                  icon: Ico.monitor,
                  title: 'No PCs yet',
                  message: 'Pair a PC to stream from it.',
                  action: PrimaryButton(
                    label: 'Pair a PC',
                    onTap: () => controller.act(const StartPairing()),
                  ),
                )
              else
                for (final pc in pcs)
                  PcRow(
                    key: ValueKey(pc.id),
                    pc: pc,
                    selected: pc.id == selectedId,
                    mobile: mobile,
                    onSelect: () => onSelect(pc),
                  ),
            ],
          ),
        ),
        const HLine(),
        _DeviceFooter(state: state, mobile: mobile),
      ],
    );
  }
}

/// The pinned "This PC" row. Hosting itself is not in this version, so without a host it says
/// so; with one it shows the host's name and run state.
class ThisPcRow extends StatelessWidget {
  const ThisPcRow({
    super.key,
    required this.host,
    this.devices,
    this.selected = false,
    this.onSelect,
  });

  final HostStatus? host;

  /// The host's Devices pipe; when set the row reflects it and opens the Devices detail.
  final DevicesController? devices;
  final bool selected;
  final VoidCallback? onSelect;

  @override
  Widget build(BuildContext context) {
    final devices = this.devices;
    if (devices != null) {
      return ListenableBuilder(
        listenable: devices,
        builder: (context, _) {
          final state = devices.state;
          final (label, kind) = switch (state.connection) {
            LinkConnecting() => ('Connecting', StatusKind.attention),
            LinkDisconnected() => ('Not connected', StatusKind.offline),
            LinkConnected() => state.relayOnline
                ? ('Relay online', StatusKind.online)
                : ('Relay offline', StatusKind.attention),
          };
          final count = state.devices.length;
          return DeviceRow(
            icon: Ico.server,
            title: state.pcName ?? 'This PC',
            status: StatusPill(label, kind: kind),
            meta: [if (state.isConnected) count == 1 ? '1 paired device' : '$count paired devices'],
            selected: selected,
            onTap: onSelect,
          );
        },
      );
    }
    final host = this.host;
    if (host == null) {
      return const DeviceRow(
        icon: Ico.server,
        title: 'Hosting',
        status: StatusPill('Not connected'),
      );
    }
    final (label, kind) = switch (host.run) {
      HostStarting() => ('Starting', StatusKind.attention),
      HostRunning() => ('Hosting', StatusKind.online),
      HostRestarting() => ('Restarting', StatusKind.attention),
      HostStopped() => ('Stopped', StatusKind.offline),
      HostNotConfigured() => ('Not set up', StatusKind.neutral),
    };
    final viewer = host.viewer;
    return DeviceRow(
      icon: Ico.server,
      title: host.pcName,
      status: StatusPill(label, kind: kind),
      meta: [if (viewer != null) '${viewer.name} connected'],
    );
  }
}

/// One of Your PCs.
class PcRow extends StatelessWidget {
  const PcRow({
    super.key,
    required this.pc,
    required this.selected,
    required this.mobile,
    required this.onSelect,
  });

  final RemotePc pc;
  final bool selected;
  final bool mobile;
  final VoidCallback onSelect;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    void connect() => controller.act(Connect(pcId: pc.id));
    return DeviceRow(
      icon: pc.kind == PcKind.laptop ? Ico.laptop : Ico.monitor,
      title: pc.name,
      status: pcStatusPill(pc),
      id: pc.id,
      meta: [pcLastText(pc)],
      selected: selected && !mobile,
      onTap: mobile ? connect : onSelect,
      trailing: mobile
          ? IconBtn(
              Ico.more,
              size: 40,
              semanticLabel: 'Actions for ${pc.name}',
              onTap: () => showPcActions(context, pc),
            )
          : (selected
              ? PrimaryButton(label: 'Connect', height: 32, icon: Ico.monitorPlay, onTap: connect)
              : SecondaryButton(label: 'Connect', height: 32, onTap: connect)),
    );
  }
}

class _DeviceFooter extends StatelessWidget {
  const _DeviceFooter({required this.state, required this.mobile});

  final AppState state;
  final bool mobile;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    final id = state.thisDevice.id;
    final thisDevice = Row(
      children: [
        Text('This device', style: ts(context.type.uiSmall, context.colors.textSecondary)),
        const SizedBox(width: 8),
        Flexible(child: CopyableText(id, size: 13)),
      ],
    );
    final pad = mobile ? 16.0 : 32.0;
    if (!mobile) {
      return Padding(
        padding: EdgeInsets.symmetric(horizontal: pad, vertical: 12),
        child: Row(
          children: [
            Expanded(child: thisDevice),
            TextBtn('Settings', icon: Ico.settings, onTap: () => openSettings(context, state)),
          ],
        ),
      );
    }
    return Padding(
      padding: EdgeInsets.fromLTRB(pad, 12, pad, 16),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          thisDevice,
          const SizedBox(height: 12),
          SecondaryButton(label: 'Settings', onTap: () => openSettings(context, state)),
          const SizedBox(height: 8),
          PrimaryButton(
            label: 'Pair a PC',
            expand: true,
            onTap: () => controller.act(const StartPairing()),
          ),
        ],
      ),
    );
  }
}
