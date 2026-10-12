package main

import (
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestHealthzAddressUsesLoopbackForUnspecifiedListen(t *testing.T) {
	for _, test := range []struct{ listen, want string }{
		{":8787", "127.0.0.1:8787"},
		{"0.0.0.0:8787", "127.0.0.1:8787"},
		{"[::]:8787", "127.0.0.1:8787"},
		{"127.0.0.1:8788", "127.0.0.1:8788"},
		{"172.30.0.5:8787", "172.30.0.5:8787"},
	} {
		got, err := healthzAddress(test.listen)
		if err != nil || got != test.want {
			t.Fatalf("healthzAddress(%q) = %q, %v; want %q", test.listen, got, err, test.want)
		}
	}
	if _, err := healthzAddress("8787"); err == nil {
		t.Fatal("listen without host:port accepted")
	}
}

func TestProbeHealthzRequiresPublicHost(t *testing.T) {
	// Mirrors the relay: /healthz answers only for the public Host header.
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Host != "play.example.invalid" || r.URL.Path != "/healthz" {
			http.NotFound(w, r)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"ok":true,"backend":"go"}`))
	}))
	defer server.Close()
	address := server.Listener.Addr().String()
	if err := probeHealthz(address, "play.example.invalid"); err != nil {
		t.Fatalf("matching Host rejected: %v", err)
	}
	if err := probeHealthz(address, "other.example.invalid"); err == nil {
		t.Fatal("wrong Host accepted")
	}
}
