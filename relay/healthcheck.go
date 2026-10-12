package main

import (
	"context"
	"errors"
	"flag"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// healthcheckCommand probes the running relay's /healthz for Docker HEALTHCHECK.
// The image is scratch and has no shell, curl or wget, so the binary does the probe.
func healthcheckCommand(args []string) error {
	f := flag.NewFlagSet("healthcheck", flag.ContinueOnError)
	configPath := f.String("config", "/data/config.json", "private relay configuration")
	if err := f.Parse(args); err != nil {
		return err
	}
	cfg, err := LoadConfig(*configPath)
	if err != nil {
		return err
	}
	publicURL, err := url.Parse(cfg.PublicURL)
	if err != nil {
		return errors.New("publicURL is invalid")
	}
	address, err := healthzAddress(cfg.Listen)
	if err != nil {
		return err
	}
	return probeHealthz(address, publicURL.Host)
}

// healthzAddress maps the relay's listen address to a loopback address the probe can dial.
// An unspecified bind host (empty, 0.0.0.0 or ::) is reached through 127.0.0.1.
func healthzAddress(listen string) (string, error) {
	host, port, err := net.SplitHostPort(listen)
	if err != nil {
		return "", errors.New("listen must be host:port")
	}
	if ip := net.ParseIP(host); host == "" || (ip != nil && ip.IsUnspecified()) {
		host = "127.0.0.1"
	}
	return net.JoinHostPort(host, port), nil
}

// probeHealthz requests /healthz with the public Host header, which the relay requires.
func probeHealthz(address, publicHost string) error {
	client := &http.Client{Timeout: 3 * time.Second}
	request, err := http.NewRequestWithContext(context.Background(), http.MethodGet, "http://"+address+"/healthz", nil)
	if err != nil {
		return errors.New("healthcheck request could not be built")
	}
	request.Host = publicHost
	response, err := client.Do(request)
	if err != nil {
		return errors.New("healthcheck request failed")
	}
	defer response.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(response.Body, 256))
	if response.StatusCode != http.StatusOK || !strings.Contains(string(body), `"ok":true`) {
		return errors.New("healthcheck returned an unhealthy response")
	}
	return nil
}
