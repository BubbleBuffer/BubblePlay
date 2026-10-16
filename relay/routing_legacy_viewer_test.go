package main

import (
	"testing"
	"time"
)

// Master Android sends only the role in hello, and only the signaling
// mediaEpoch on auxiliary URLs. It ignores the lane's pc and ticket fields.
func TestRoutingPCLessViewerReachesIDHost(t *testing.T) {
	f := newRelayFixture(t)
	pc := routingNewPC(t)
	host, _ := routingRegister(t, f, pc, f.cfg.Token)
	headers := routingViewerHeaders(f)
	headers.Del("Origin") // Native Android sends cookies without Origin.
	viewer, hello := routingOpenHello(t, f, "/ws", headers, map[string]any{"type": "hello", "role": "client"})
	if hello["pc"] != pc.id {
		t.Fatalf("wrong default PC: %s", routingSummary(hello))
	}
	routingSend(t, viewer, map[string]any{"type": "offer", "sdp": "old-android-offer"})
	routingExpectSDP(t, host, "old-android-offer")
	routingSend(t, host, map[string]any{"type": "answer", "sdp": "host-answer"})
	routingExpectSDP(t, viewer, "host-answer")
	epoch := routingEpoch(t, hello)
	for _, kind := range []string{"media", "file"} {
		client, ok := routingOpenHello(t, f, "/"+kind+"-ws?epoch="+epoch,
			headers, map[string]any{"type": "hello", "role": kind + "-client"})
		routingAdmitted(t, ok)
		routingExpectPresence(t, client, kind, false)
		dataHost, ok := routingOpenHello(t, f, "/host-"+kind+"-ws?epoch="+epoch+"&pc="+pc.id,
			routingBearer(f.cfg.HostGateToken), map[string]any{"type": "hello", "role": kind + "-host"})
		routingAdmitted(t, ok)
		routingExpectPresence(t, dataHost, kind, true)
		routingExpectPresence(t, client, kind, true)
		if kind == "media" {
			routingWriteBinary(t, dataHost, routingMediaFrame(t, 0xA1))
			routingExpectBinary(t, client, routingMediaFrame(t, 0xA1))
			routingSend(t, client, packetFixture(t).Ack)
			if readJSON(t, dataHost)["recvTsMs"] != 12.3 {
				t.Fatal("media ACK did not reach the ID host")
			}
		} else {
			routingWriteBinary(t, client, routingFilePacket(t, 0))
			routingExpectBinary(t, dataHost, routingFilePacket(t, 0))
			routingWriteBinary(t, dataHost, routingFilePacket(t, 1))
			routingExpectBinary(t, client, routingFilePacket(t, 1))
		}
	}
}

func TestRoutingPCLessAuxiliaryRefusesChangedDefault(t *testing.T) {
	f := newRelayFixture(t)
	a, b := routingNewPC(t), routingNewPC(t)
	routingClock(f, routingBase)
	hostA, _ := routingRegister(t, f, a, f.cfg.Token)
	headers := routingViewerHeaders(f)
	headers.Del("Origin")
	viewerA, helloA := routingOpenHello(t, f, "/ws", headers, map[string]any{"type": "hello", "role": "client"})
	epochA := routingEpoch(t, helloA)
	routingClock(f, routingBase.Add(time.Minute))
	routingRegister(t, f, b, f.cfg.Token)
	// B has a live signaling epoch too: A's auxiliary sockets must not join it.
	_, helloB := routingViewer(t, f, "/ws?pc="+b.id)
	if routingEpoch(t, helloB) == epochA {
		t.Fatal("signaling epochs were reused across PCs")
	}
	_, defaultHello := routingViewer(t, f, "/ws")
	if defaultHello["pc"] != b.id {
		t.Fatal("second registration did not change the default")
	}
	for _, kind := range []string{"media", "file"} {
		routingRefusedUpgrade(t, f, "/"+kind+"-ws?epoch="+epochA, headers)
		// Explicit targeting keeps its existing behavior despite default churn.
		_, dataHello := routingOpenHello(t, f, "/"+kind+"-ws?epoch="+epochA+"&pc="+a.id,
			routingViewerHeaders(f), map[string]any{"type": "hello", "role": kind + "-client"})
		routingAdmitted(t, dataHello)
	}
	// Refusal neither moves nor disconnects the original signaling peer.
	routingSend(t, viewerA, map[string]any{"type": "offer", "sdp": "still-on-a"})
	routingExpectSDP(t, hostA, "still-on-a")
}
