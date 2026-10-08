package main

import (
	"crypto/subtle"
	"encoding/json"
	"errors"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
)

type Config struct {
	PublicURL             string   `json:"publicURL"`
	SourceURL             string   `json:"sourceURL,omitempty"`
	AssetsDir             string   `json:"assetsDir"`
	StateDir              string   `json:"stateDir"`
	Listen                string   `json:"listen"`
	TrustedProxyCIDRs     []string `json:"trustedProxyCIDRs,omitempty"`
	Token                 string   `json:"token"`
	HostGateToken         string   `json:"hostGateToken"`
	AdminToken            string   `json:"adminToken"`
	PasswordHash          string   `json:"passwordHash"`
	ApprovalMethods       []string `json:"approvalMethods"`
	TOTPSecret            string   `json:"totpSecret,omitempty"`
	NtfyURL               string   `json:"ntfyURL,omitempty"`
	NtfyTopic             string   `json:"ntfyTopic,omitempty"`
	NtfyToken             string   `json:"ntfyToken,omitempty"`
	ApprovalTTLSeconds    int      `json:"approvalTTLSeconds"`
	TurnHost              string   `json:"turnHost,omitempty"`
	TurnSecret            string   `json:"turnSecret,omitempty"`
	TurnTTLSeconds        int      `json:"turnTTLSeconds,omitempty"`
	AllowInsecureLoopback bool     `json:"allowInsecureLoopback,omitempty"`
	MaxConnections        int      `json:"maxConnections"`
}

const defaultSourceURL = "https://github.com/BubbleBuffer/BubblePlay"

func (c Config) sourceURL() string {
	u, err := url.Parse(c.SourceURL)
	if err == nil && u.Host != "" && u.User == nil && (u.Scheme == "https" || u.Scheme == "http") {
		return u.String()
	}
	return defaultSourceURL
}

func LoadConfig(path string) (Config, error) {
	var c Config
	f, err := os.Open(path)
	if err != nil {
		return c, errors.New("cannot read relay configuration")
	}
	defer f.Close()
	d := json.NewDecoder(http.MaxBytesReader(nil, f, 64*1024))
	d.DisallowUnknownFields()
	if err = d.Decode(&c); err != nil {
		return c, errors.New("invalid relay configuration JSON")
	}
	return c, c.Validate()
}
func (c *Config) Validate() error {
	u, err := url.Parse(c.PublicURL)
	if err != nil || u.Host == "" || u.User != nil || u.RawQuery != "" || u.Fragment != "" || (u.Path != "" && u.Path != "/") {
		return errors.New("publicURL must be an HTTPS origin")
	}
	if u.Scheme != "https" && !(c.AllowInsecureLoopback && u.Scheme == "http" && isLoopbackHost(u.Hostname())) {
		return errors.New("publicURL requires HTTPS; insecure loopback is development only")
	}
	c.PublicURL = u.Scheme + "://" + u.Host
	for _, value := range []string{c.Token, c.HostGateToken, c.AdminToken} {
		if len(value) < 32 || len(value) > 8192 || strings.ContainsAny(value, "\r\n") {
			return errors.New("relay credentials must contain at least 32 characters")
		}
	}
	if !strings.HasPrefix(c.PasswordHash, "$argon2id$") {
		return errors.New("configure an Argon2id password hash with relay init")
	}
	if err := c.validateApprovalConfig(); err != nil {
		return err
	}
	for _, cidr := range c.TrustedProxyCIDRs {
		if _, _, err := net.ParseCIDR(cidr); err != nil {
			return errors.New("invalid trusted proxy CIDR")
		}
	}
	if c.Listen == "" {
		c.Listen = "127.0.0.1:8787"
	}
	if _, _, err = net.SplitHostPort(c.Listen); err != nil {
		return errors.New("listen must be host:port")
	}
	if u.Scheme == "http" {
		host, _, _ := net.SplitHostPort(c.Listen)
		if !isLoopbackHost(host) {
			return errors.New("insecure development HTTP must bind only to loopback")
		}
	}
	if c.StateDir == "" || c.AssetsDir == "" {
		return errors.New("stateDir and assetsDir are required")
	}
	if c.MaxConnections == 0 {
		c.MaxConnections = 32
	}
	if c.MaxConnections < 6 || c.MaxConnections > 128 {
		return errors.New("maxConnections must be 6..128")
	}
	if c.ApprovalTTLSeconds == 0 {
		c.ApprovalTTLSeconds = 90
	}
	if c.ApprovalTTLSeconds < 30 || c.ApprovalTTLSeconds > 120 {
		return errors.New("approval TTL must be 30..120 seconds")
	}
	if c.TurnTTLSeconds == 0 {
		c.TurnTTLSeconds = 21600
	}
	if c.TurnTTLSeconds < 300 || c.TurnTTLSeconds > 86400 {
		return errors.New("TURN TTL must be 300..86400 seconds")
	}
	if (c.TurnHost == "") != (c.TurnSecret == "") {
		return errors.New("TURN host and secret must be configured together")
	}
	if c.TurnHost != "" {
		if _, err := turnAuthority(c.TurnHost); err != nil {
			return err
		}
	}
	return nil
}

func validateApprovalMethods(methods []string) error {
	if len(methods) == 0 {
		return errors.New("approvalMethods must enable at least one second factor")
	}
	seen := make(map[string]bool, len(methods))
	for _, method := range methods {
		if method != "totp" && method != "ntfy" && method != "host" {
			return errors.New("approvalMethods contains an unknown method")
		}
		if seen[method] {
			return errors.New("approvalMethods contains a duplicate method")
		}
		seen[method] = true
	}
	return nil
}

func (c Config) validateApprovalConfig() error {
	if err := validateApprovalMethods(c.effectiveApprovalMethods()); err != nil {
		return err
	}
	if c.methodEnabled("totp") && c.TOTPSecret == "" {
		return errors.New("totp approval requires an authenticator secret")
	}
	if c.methodEnabled("ntfy") && (c.NtfyURL == "" || c.NtfyTopic == "" || c.NtfyToken == "") {
		return errors.New("ntfy approval requires URL, topic and token")
	}
	if c.methodEnabled("ntfy") && (len(c.NtfyToken) > 8192 || strings.ContainsAny(c.NtfyToken, "\r\n")) {
		return errors.New("ntfy token must be a single nonempty value of at most 8192 bytes")
	}
	if c.NtfyURL != "" {
		n, e := url.Parse(c.NtfyURL)
		if e != nil || n.Host == "" || n.User != nil || n.RawQuery != "" || n.Fragment != "" || (n.Scheme != "https" && !(c.AllowInsecureLoopback && n.Scheme == "http" && isLoopbackHost(n.Hostname()))) {
			return errors.New("ntfyURL must use HTTPS")
		}
	}
	return nil
}

// A missing methods list retains the behavior of configurations predating it.
func (c Config) effectiveApprovalMethods() []string {
	if c.ApprovalMethods != nil {
		return c.ApprovalMethods
	}
	methods := make([]string, 0, 2)
	if c.TOTPSecret != "" {
		methods = append(methods, "totp")
	}
	if c.NtfyURL != "" || c.NtfyTopic != "" || c.NtfyToken != "" {
		methods = append(methods, "ntfy")
	}
	return methods
}

func (c Config) methodEnabled(name string) bool {
	for _, method := range c.effectiveApprovalMethods() {
		if method == name {
			return true
		}
	}
	return false
}

func (c Config) approvalNotifiers() []string {
	methods := make([]string, 0, 2)
	for _, method := range c.effectiveApprovalMethods() {
		if method == "ntfy" || method == "host" {
			methods = append(methods, method)
		}
	}
	return methods
}
func isLoopbackHost(host string) bool {
	return host == "localhost" || (net.ParseIP(host) != nil && net.ParseIP(host).IsLoopback())
}
func SameOrigin(r *http.Request, c Config, allowAbsent bool) bool {
	origin := r.Header.Get("Origin")
	if origin == "" {
		return allowAbsent
	}
	return origin == c.PublicURL
}
func ClientIP(r *http.Request, c Config) string {
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		host = r.RemoteAddr
	}
	ip := net.ParseIP(host)
	trusted := false
	for _, cidr := range c.TrustedProxyCIDRs {
		_, network, e := net.ParseCIDR(cidr)
		if e == nil && network.Contains(ip) {
			trusted = true
			break
		}
	}
	if trusted {
		// Walk right-to-left so a client-supplied prefix cannot impersonate another client.
		parts := strings.Split(r.Header.Get("X-Forwarded-For"), ",")
		if len(parts) > 16 {
			return host
		}
		for i := len(parts) - 1; i >= 0; i-- {
			candidate := net.ParseIP(strings.TrimSpace(parts[i]))
			if candidate == nil {
				return host
			}
			inside := false
			for _, cidr := range c.TrustedProxyCIDRs {
				_, n, _ := net.ParseCIDR(cidr)
				if n.Contains(candidate) {
					inside = true
					break
				}
			}
			if !inside {
				return candidate.String()
			}
		}
	}
	return host
}
func secureEqual(a, b string) bool {
	return len(a) == len(b) && subtle.ConstantTimeCompare([]byte(a), []byte(b)) == 1
}
func savePrivateJSON(path string, value any) error {
	bytes, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		return err
	}
	bytes = append(bytes, '\n')
	return savePrivateBytes(path, bytes)
}
func savePrivateBytes(path string, bytes []byte) error {
	if err := ensurePrivateDirectory(filepath.Dir(path)); err != nil {
		return err
	}
	if info, err := os.Lstat(path); err == nil && (!info.Mode().IsRegular() || info.Mode()&os.ModeSymlink != 0) {
		return errors.New("private file must be a regular file")
	}
	f, err := os.CreateTemp(filepath.Dir(path), ".config-*")
	if err != nil {
		return err
	}
	name := f.Name()
	defer os.Remove(name)
	if err = f.Chmod(0600); err == nil {
		_, err = f.Write(bytes)
	}
	if err == nil {
		err = f.Sync()
	}
	closeErr := f.Close()
	if err != nil {
		return err
	}
	if closeErr != nil {
		return closeErr
	}
	return os.Rename(name, path)
}
func ensurePrivateDirectory(path string) error {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return err
	}
	for part := absolute; ; part = filepath.Dir(part) {
		info, err := os.Lstat(part)
		if err == nil && (info.Mode()&os.ModeSymlink != 0 || !info.IsDir()) {
			return errors.New("private state path must not contain symlinks or non-directories")
		}
		if err != nil && !os.IsNotExist(err) {
			return err
		}
		if filepath.Dir(part) == part {
			break
		}
	}
	if err := os.MkdirAll(absolute, 0700); err != nil {
		return err
	}
	return os.Chmod(absolute, 0700)
}
