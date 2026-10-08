package main

import (
	"crypto/rand"
	"encoding/base32"
	"encoding/base64"
	"flag"
	"fmt"
	"io"
	"net/url"
	"os"
	"path/filepath"
	"strings"

	"golang.org/x/term"
)

func randomCredential(bytes int) string {
	value := make([]byte, bytes)
	if _, err := rand.Read(value); err != nil {
		panic("system random source unavailable")
	}
	return base64.RawURLEncoding.EncodeToString(value)
}
func newTOTPSecret() string {
	value := make([]byte, 20)
	if _, err := rand.Read(value); err != nil {
		panic("system random source unavailable")
	}
	return base32.StdEncoding.WithPadding(base32.NoPadding).EncodeToString(value)
}
func readPassword(file string) (string, error) {
	if file != "" {
		return readSecretFile(file)
	}
	if !term.IsTerminal(int(os.Stdin.Fd())) {
		return "", fmt.Errorf("use an interactive terminal or --password-file with a private file")
	}
	fmt.Fprint(os.Stderr, "New application password: ")
	first, err := term.ReadPassword(int(os.Stdin.Fd()))
	fmt.Fprintln(os.Stderr)
	if err != nil {
		return "", err
	}
	fmt.Fprint(os.Stderr, "Repeat password: ")
	second, err := term.ReadPassword(int(os.Stdin.Fd()))
	fmt.Fprintln(os.Stderr)
	if err != nil {
		return "", err
	}
	if !secureEqual(string(first), string(second)) {
		return "", fmt.Errorf("passwords did not match")
	}
	return string(first), nil
}

func readSecretFile(file string) (string, error) {
	f, err := os.Open(file)
	if err != nil {
		return "", fmt.Errorf("cannot read private secret file")
	}
	defer f.Close()
	data, err := io.ReadAll(io.LimitReader(f, 8195))
	if err != nil || len(data) > 8194 {
		return "", fmt.Errorf("private secret file is too large or unreadable")
	}
	return strings.TrimRight(string(data), "\r\n"), nil
}

func readNtfyToken(file string) (string, error) {
	if file != "" {
		return readSecretFile(file)
	}
	if !term.IsTerminal(int(os.Stdin.Fd())) {
		return "", fmt.Errorf("use an interactive terminal or --ntfy-token-file with a private file")
	}
	fmt.Fprint(os.Stderr, "ntfy publishing token: ")
	data, err := term.ReadPassword(int(os.Stdin.Fd()))
	fmt.Fprintln(os.Stderr)
	return string(data), err
}

type trustedProxyFlags []string

func (values *trustedProxyFlags) String() string { return strings.Join(*values, ",") }
func (values *trustedProxyFlags) Set(value string) error {
	*values = append(*values, value)
	return nil
}

func parseApprovalMethods(value string) ([]string, error) {
	methods := strings.Split(value, ",")
	for i := range methods {
		methods[i] = strings.TrimSpace(methods[i])
	}
	return methods, validateApprovalMethods(methods)
}

func setupCommand(command string, args []string) error {
	if command != "init" && command != "rotate" {
		return fmt.Errorf("unsupported setup command")
	}
	f := flag.NewFlagSet(command, flag.ContinueOnError)
	dir := f.String("state-dir", "", "private writable state directory")
	origin := f.String("public-url", "", "public HTTPS origin")
	assets := f.String("assets-dir", "/assets", "browser assets directory")
	listen := f.String("listen", ":8787", "listen address behind HTTPS proxy")
	insecure := f.Bool("insecure-loopback", false, "development only: permit HTTP on an explicitly loopback-bound listener")
	passwordFile := f.String("password-file", "", "private file; interactive prompt when omitted")
	methods := f.String("methods", "totp", "comma-separated approval methods: totp,ntfy,host")
	var trustedProxies trustedProxyFlags
	f.Var(&trustedProxies, "trusted-proxy", "trusted proxy CIDR during init; may be repeated")
	ntfyURL := f.String("ntfy-url", "", "HTTPS ntfy JSON publishing endpoint")
	ntfyTopic := f.String("ntfy-topic", "", "ntfy topic; token is prompted without echo")
	ntfyTokenFile := f.String("ntfy-token-file", "", "private ntfy token file for noninteractive setup")
	rotateTOTP := f.Bool("totp", false, "replace authenticator secret during rotate")
	rotatePassword := f.Bool("password", false, "replace password during rotate")
	if err := f.Parse(args); err != nil {
		return err
	}
	if f.NArg() != 0 {
		return fmt.Errorf("unexpected setup arguments")
	}
	methodsSet, ntfyFlagsSet := false, false
	f.Visit(func(value *flag.Flag) {
		switch value.Name {
		case "methods":
			methodsSet = true
		case "ntfy-url", "ntfy-topic", "ntfy-token-file":
			ntfyFlagsSet = true
		}
	})
	var selectedMethods []string
	var err error
	if command == "init" || methodsSet {
		selectedMethods, err = parseApprovalMethods(*methods)
		if err != nil {
			return err
		}
	}
	if *dir == "" {
		return fmt.Errorf("--state-dir is required")
	}
	absolute, err := filepath.Abs(*dir)
	if err != nil {
		return err
	}
	path := filepath.Join(absolute, "config.json")
	var cfg Config
	if command == "init" {
		if _, err := os.Stat(path); err == nil {
			return fmt.Errorf("configuration already exists; use rotate")
		}
		if *origin == "" {
			return fmt.Errorf("--public-url is required")
		}
		cfg = Config{PublicURL: *origin, StateDir: absolute, AssetsDir: *assets, Listen: *listen, AllowInsecureLoopback: *insecure, TrustedProxyCIDRs: trustedProxies, ApprovalMethods: selectedMethods, Token: randomCredential(32), HostGateToken: randomCredential(32), AdminToken: randomCredential(32), ApprovalTTLSeconds: 90, MaxConnections: 32}
		if cfg.methodEnabled("totp") {
			cfg.TOTPSecret = newTOTPSecret()
		}
		*rotatePassword = true
	} else {
		if len(trustedProxies) != 0 {
			return fmt.Errorf("--trusted-proxy is supported during init only")
		}
		cfg, err = LoadConfig(path)
		if err != nil {
			return err
		}
		if !*rotatePassword && !*rotateTOTP && !methodsSet {
			return fmt.Errorf("rotate requires --password, --totp and/or --methods; restart the server after rotation")
		}
		if methodsSet {
			cfg.ApprovalMethods = selectedMethods
		}
		if *rotateTOTP && !cfg.methodEnabled("totp") {
			return fmt.Errorf("--totp requires totp to be enabled; use --methods to enable it")
		}
		if !cfg.methodEnabled("totp") {
			cfg.TOTPSecret = ""
		} else if *rotateTOTP || cfg.TOTPSecret == "" {
			cfg.TOTPSecret = newTOTPSecret()
		}
	}
	if ntfyFlagsSet && !cfg.methodEnabled("ntfy") {
		return fmt.Errorf("ntfy flags require ntfy in --methods")
	}
	if cfg.methodEnabled("ntfy") {
		if *ntfyURL != "" {
			cfg.NtfyURL = *ntfyURL
		}
		if *ntfyTopic != "" {
			cfg.NtfyTopic = *ntfyTopic
		}
		if cfg.NtfyURL == "" || cfg.NtfyTopic == "" {
			return fmt.Errorf("ntfy approval requires --ntfy-url and --ntfy-topic")
		}
		if *ntfyTokenFile != "" || cfg.NtfyToken == "" {
			cfg.NtfyToken, err = readNtfyToken(*ntfyTokenFile)
			if err != nil {
				return err
			}
		}
		if len(cfg.NtfyToken) == 0 || len(cfg.NtfyToken) > 8192 || strings.ContainsAny(cfg.NtfyToken, "\r\n") {
			return fmt.Errorf("ntfy token must be a single nonempty value of at most 8192 bytes")
		}
	}
	if *rotatePassword {
		password, err := readPassword(*passwordFile)
		if err != nil {
			return err
		}
		if !validNewPassword(password) {
			return fmt.Errorf("password must contain 12..256 characters")
		}
		cfg.PasswordHash, err = HashPassword(password)
		if err != nil {
			return err
		}
	}
	if err = cfg.Validate(); err != nil {
		return err
	}
	if err = os.MkdirAll(absolute, 0700); err != nil {
		return err
	}
	if err = savePrivateJSON(path, cfg); err != nil {
		return err
	}
	if err = writeProvision(cfg); err != nil {
		return err
	}
	fmt.Fprintf(os.Stdout, "Private configuration saved to %s\nPrivate host credentials and approval provisioning saved to %s\n", path, filepath.Join(absolute, "provision.txt"))
	if command == "rotate" {
		fmt.Fprintln(os.Stdout, "Restart the relay to load the rotated configuration. Existing application sessions are invalidated by credential rotation.")
	}
	return nil
}
func writeProvision(cfg Config) error {
	u, _ := url.Parse(cfg.PublicURL)
	text := fmt.Sprintf("PRIVATE — never publish this file or configuration.\nHost endpoint: wss://%s/host-ws\nTOKEN: %s\nHOST_GATE_TOKEN: %s\nGATE_ADMIN_TOKEN: %s\nApproval methods: %s\n", u.Host, cfg.Token, cfg.HostGateToken, cfg.AdminToken, strings.Join(cfg.effectiveApprovalMethods(), ","))
	if cfg.methodEnabled("totp") {
		q := url.Values{"secret": {cfg.TOTPSecret}, "issuer": {"BubblePlay"}, "algorithm": {"SHA1"}, "digits": {"6"}, "period": {"30"}}
		enrollment := "otpauth://totp/" + url.PathEscape("BubblePlay:"+u.Host) + "?" + q.Encode()
		text += fmt.Sprintf("Authenticator enrollment URI: %s\nAuthenticator secret: %s\n", enrollment, cfg.TOTPSecret)
	}
	return savePrivateBytes(filepath.Join(cfg.StateDir, "provision.txt"), []byte(text))
}
