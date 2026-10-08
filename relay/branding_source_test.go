package main

import (
	"encoding/json"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestSourceURL(t *testing.T) {
	for _, value := range []string{"", "javascript:alert(1)", "file:///tmp/source", "https://user:password@example.org/"} {
		if got := (Config{SourceURL: value}).sourceURL(); got != defaultSourceURL {
			t.Fatalf("unsafe URL %q became %q", value, got)
		}
	}
	const modified = "https://example.org/modified-source"
	f := newRelayFixture(t)
	f.relay.auth.cfg.SourceURL = modified
	response, err := f.server.Client().Get(f.server.URL + "/__auth/methods")
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	var data map[string]any
	if err := json.NewDecoder(response.Body).Decode(&data); err != nil {
		t.Fatal(err)
	}
	if data["sourceURL"] != modified || response.Header.Get("Cache-Control") != "no-store" {
		t.Fatalf("source discovery: %v", data)
	}
}

func TestPublicFaviconsAndLoginMark(t *testing.T) {
	f := newRelayFixture(t)
	for _, path := range []string{"/favicon.svg", "/favicon-32.png"} {
		if err := os.WriteFile(filepath.Join(f.cfg.AssetsDir, path[1:]), []byte("synthetic favicon"), 0600); err != nil {
			t.Fatal(err)
		}
		response, err := f.server.Client().Get(f.server.URL + path)
		if err != nil {
			t.Fatal(err)
		}
		response.Body.Close()
		if response.StatusCode != 200 {
			t.Fatalf("%s status %d", path, response.StatusCode)
		}
	}
	for _, path := range []string{"/favicon.ico", "/favicon-other.png", "/receiver.js", "/login.html"} {
		if publicLoginAsset(path) {
			t.Fatalf("unexpected public asset %s", path)
		}
	}
	request, _ := http.NewRequest("GET", f.server.URL+"/", nil)
	request.Header.Set("Accept", "text/html")
	response, err := f.server.Client().Do(request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	body, _ := io.ReadAll(response.Body)
	if !strings.Contains(string(body), `aria-label="BubblePlay mark"`) || !strings.Contains(response.Header.Get("Content-Security-Policy"), "img-src 'self'") {
		t.Fatal("login mark or favicon CSP missing")
	}
}
