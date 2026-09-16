//go:build js && wasm

package main

import (
	"flag"
	"fmt"
	"os"
	"regexp"
	"strconv"
	"strings"
)

type config struct {
	memoryMB    int
	cmdline     string
	jitDisabled bool
	benchmark   bool
	netdev      string
}

func parseFlags(args []string) (config, error) {
	var mem, appendArgs, export, netdev string
	f := flag.NewFlagSet("rv64", flag.ContinueOnError)
	f.StringVar(&mem, "m", "512M", "memory size")
	f.StringVar(&mem, "mem", "512M", "memory size")
	f.StringVar(&appendArgs, "append", "", "kernel command line additions")
	f.StringVar(&export, "export", "", "guest device used for host export")
	f.StringVar(&netdev, "netdev", "", "network device configuration (user,type=virtio,relay_url=...)")
	if err := f.Parse(args); err != nil {
		return config{}, err
	}
	if value := os.Getenv("VM_APPEND"); value != "" {
		appendArgs = value
	}
	jitDisabled := false
	benchmark := false
	kernelArgs := make([]string, 0, len(strings.Fields(appendArgs)))
	for _, arg := range strings.Fields(appendArgs) {
		if arg == "rv64.jit=off" {
			jitDisabled = true
			continue
		}
		if arg == "rv64.benchmark=1" {
			benchmark = true
		}
		if strings.HasPrefix(arg, "rv64.static-t0=") {
			return config{}, fmt.Errorf("rv64.static-t0 was removed after the experiment was rejected")
		}
		kernelArgs = append(kernelArgs, arg)
	}
	appendArgs = strings.Join(kernelArgs, " ")
	bytes, err := parseMemorySize(mem)
	if err != nil {
		return config{}, err
	}
	// rv64.network=fetch is the default zero-infrastructure backend. When a
	// netdev arrives (host passes -netdev "user,type=virtio,relay_url=wss://…"
	// from <wanix-vm netdev>), the adapter switches the emulator to the vnet
	// wsproxy tunnel instead — same as the v86 plugin — and the guest should
	// NOT hardcode the fetch proxy.
	cmdline := "console=hvc0 loglevel=3 init=/bin/init rw root=host9p rootfstype=9p rootflags=trans=virtio,version=9p2000.L,cache=loose,access=any,msize=131072"
	if netdev == "" {
		cmdline += " rv64.network=fetch"
	}
	if appendArgs != "" {
		cmdline += " " + appendArgs
	}
	if export != "" {
		// Mirror the copy/v86 layout: hvc0 (virtio console) is the interactive
		// terminal and the 8250 UART (ttyS0) carries the WANIX host-export
		// stream.
		cmdline += " export=" + export
	}
	return config{
		memoryMB:    bytes / (1024 * 1024),
		cmdline:     cmdline,
		jitDisabled: jitDisabled,
		benchmark:   benchmark,
		netdev:      netdev,
	}, nil
}

func parseMemorySize(value string) (int, error) {
	m := regexp.MustCompile(`^(\d+)([KMGT]?)$`).FindStringSubmatch(strings.ToUpper(value))
	if m == nil {
		return 0, fmt.Errorf("invalid memory size %q", value)
	}
	n, err := strconv.Atoi(m[1])
	if err != nil {
		return 0, err
	}
	multipliers := map[string]int{"": 1024 * 1024, "K": 1024, "M": 1024 * 1024, "G": 1024 * 1024 * 1024}
	return n * multipliers[m[2]], nil
}
