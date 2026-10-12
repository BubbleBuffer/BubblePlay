package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"
)

func main() {
	if err := run(os.Args[1:]); err != nil {
		log.Print(err)
		os.Exit(1)
	}
}
func run(args []string) error {
	if len(args) > 0 && (args[0] == "init" || args[0] == "rotate") {
		return setupCommand(args[0], args[1:])
	}
	if len(args) > 0 && args[0] == "healthcheck" {
		return healthcheckCommand(args[1:])
	}
	f := flag.NewFlagSet("bubbleplay-relay", flag.ContinueOnError)
	configPath := f.String("config", "/data/config.json", "private relay configuration")
	if err := f.Parse(args); err != nil {
		return err
	}
	cfg, err := LoadConfig(*configPath)
	if err != nil {
		return err
	}
	relay, err := NewRelay(cfg)
	if err != nil {
		return err
	}
	defer relay.Close()
	server := &http.Server{Addr: cfg.Listen, Handler: relay, ReadHeaderTimeout: 5 * time.Second, ReadTimeout: 15 * time.Second, WriteTimeout: 130 * time.Second, IdleTimeout: 60 * time.Second, MaxHeaderBytes: 16 * 1024}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	go func() {
		<-ctx.Done()
		relay.Close()
		shutdown, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = server.Shutdown(shutdown)
	}()
	fmt.Fprintln(os.Stderr, "BubblePlay relay ready; configuration and credentials are not logged.")
	err = server.ListenAndServe()
	if err == http.ErrServerClosed {
		return nil
	}
	return err
}
