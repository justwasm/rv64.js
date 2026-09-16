//go:build js && wasm

package main

import (
	"encoding/binary"
	"fmt"
	"io"
	"log"
	"os"
	"path"
	"strconv"
	"strings"
	"syscall/js"
)

const defaultRelayURL = "wss://rv64-http-relay.darren-e4d.workers.dev/relay"

func main() {
	cfg, err := parseFlags(os.Args[1:])
	if err != nil {
		log.Fatal(err)
	}
	sys := js.Global().Get("sys")
	writeHost(sys, "[rv64 adapter] loading runtime assets\n")
	assetRoot := path.Dir(os.Args[0])
	loader, err := await(sys.Call("readFile", path.Join(assetRoot, "rv64.js")))
	if err != nil {
		log.Fatal(err)
	}
	wasm, err := await(sys.Call("readFile", path.Join(assetRoot, "rv64_wasm.wasm")))
	if err != nil {
		log.Fatal(err)
	}
	kernel, err := await(sys.Call("readFile", "boot/Image"))
	if err != nil {
		log.Fatal(err)
	}

	module := importLibrary(loader)
	p9 := newP9Handler()
	exportPort, exportOutput := newExportChannel()
	relayURL := os.Getenv("RV64_RELAY_URL")
	if relayURL == "" {
		relayURL = defaultRelayURL
	}
	consoleOutput := js.Global().Call(
		"eval",
		"(sys) => (bytes) => { void sys.write(1, bytes); }",
	).Invoke(sys)

	events := map[string]any{
		// A pure JavaScript callback avoids re-entering the Go Wasm runtime
		// from the emulator's host_write import while generated code is live.
		// Mirror the copy/v86 layout: hvc0 (virtio console) is the interactive
		// terminal and ttyS0 (8250 UART) is the WANIX host-export link.
		"console": js.FuncOf(func(_ js.Value, args []js.Value) any {
			exportOutput(args[0])
			return nil
		}),
		"export": consoleOutput,
		"error": js.FuncOf(func(_ js.Value, args []js.Value) any {
			writeHost(sys, fmt.Sprintf("[rv64 adapter] VM error: %s\n", args[0].Call("toString").String()))
			return nil
		}),
		"stop": js.FuncOf(func(_ js.Value, args []js.Value) any {
			writeHost(sys, fmt.Sprintf("[rv64 adapter] VM stopped: %s\n", args[0].Get("reason").String()))
			return nil
		}),
	}

	// When the host passes a vnet netdev (e.g.
	// -netdev "user,type=virtio,relay_url=wss://vnet…") the adapter drives the
	// guest NIC through the same WebSocket layer-2 relay the v86 plugin uses
	// (rv64.js network mode "wsproxy"). Without one it falls back to the
	// zero-infrastructure fetch HTTP proxy.
	network := map[string]any{"mode": "fetch", "relayURL": relayURL}
	if url := parseNetdevRelayURL(cfg.netdev); url != "" {
		network = map[string]any{"mode": "wsproxy", "url": url}
		if mac := parseNetdevMac(cfg.netdev); mac != nil {
			// A per-instance MAC (the host injects "mac=" into the netdev) lets
			// the vnet DHCP pool give each concurrent guest a distinct IP
			// instead of mapping every default MAC to one address.
			u8 := js.Global().Get("Uint8Array").New(len(mac))
			js.CopyBytesToJS(u8, mac)
			network["mac"] = u8
		}
	}

	opts := map[string]any{
		"wasm":      wasm,
		"memoryMB":  cfg.memoryMB,
		"execution": map[string]any{"mode": "local"},
		"network":   network,
		"boot": map[string]any{
			"mode": "linux-direct", "kernel": kernel, "cmdline": cfg.cmdline,
			"p9":            map[string]any{"tag": "host9p", "handle": p9},
			"virtioConsole": true,
		},
		"events": events,
	}

	vm, err := await(module.Get("RV64").Call("create", opts))
	if err != nil {
		log.Fatal(err)
	}
	if cfg.jitDisabled {
		if vm.Get("setJitEnabled").Type() != js.TypeFunction {
			log.Fatal("rv64.jit=off requested, but this runtime cannot disable its JIT")
		}
		vm.Call("setJitEnabled", false)
		writeHost(sys, "[rv64 adapter] JIT disabled; running interpreter only\n")
	}
	// The comparison harness attaches to this dedicated Worker through CDP.
	// Keep the VM private in ordinary embeddings; benchmark=1 explicitly opts
	// into read-only counter observation without periodic logging or timer noise.
	if cfg.benchmark {
		js.Global().Set("__rv64BenchmarkVM", vm)
	}
	writeHost(sys, "[rv64 adapter] emulator created; starting scheduler\n")
	exportPort.Set("onmessage", js.FuncOf(func(_ js.Value, args []js.Value) any {
		vm.Get("serial").Call("send", args[0].Get("data"))
		return nil
	}))
	if _, err := await(vm.Call("start")); err != nil {
		log.Fatal(err)
	}
	// Default console size until the shell's wanix-term publishes a winch
	// frame (the emulator's DRIVER_OK path already seeds 80x24, so this only
	// covers the gap before the first resize).
	vm.Call("resize", 80, 24)
	go forwardWinch(vm)
	writeHost(sys, "[rv64 adapter] scheduler running; guest console is hvc0\n")
	js.Global().Get("self").Call("postMessage", map[string]any{
		"vm": os.Getenv("vm"), "export": exportPort,
	}, []any{exportPort})

	go copyStdin(vm)
	select {}
}

// parseNetdevRelayURL extracts the relay_url from a qemu-style netdev
// string ("user,type=virtio,relay_url=wss://…"), returning "" when no
// netdev is given or it does not carry a relay_url. This lets the adapter
// route a host-supplied vnet tunnel to the guest NIC via rv64's wsproxy
// mode, mirroring the v86 plugin's user-mode-net on the shared gateway.
func parseNetdevRelayURL(netdev string) string {
	for _, part := range strings.Split(netdev, ",") {
		kv := strings.SplitN(part, "=", 2)
		if len(kv) == 2 && kv[0] == "relay_url" && kv[1] != "" {
			return kv[1]
		}
	}
	return ""
}

// parseNetdevMac extracts a "mac=xx:xx:xx:xx:xx:xx" value from the netdev
// and returns it as a 6-byte slice, or nil when absent/unparseable.
func parseNetdevMac(netdev string) []byte {
	for _, part := range strings.Split(netdev, ",") {
		kv := strings.SplitN(part, "=", 2)
		if len(kv) != 2 || kv[0] != "mac" {
			continue
		}
		hexes := strings.Split(kv[1], ":")
		if len(hexes) != 6 {
			return nil
		}
		out := make([]byte, 6)
		for i, h := range hexes {
			v, err := strconv.ParseUint(h, 16, 8)
			if err != nil {
				return nil
			}
			out[i] = byte(v)
		}
		return out
	}
	return nil
}

func writeHost(sys js.Value, text string) {
	buf := js.Global().Get("Uint8Array").New(len(text))
	js.CopyBytesToJS(buf, []byte(text))
	sys.Call("write", 1, buf)
}

func importLibrary(source js.Value) js.Value {
	blob := js.Global().Get("Blob").New([]any{source}, map[string]any{"type": "application/javascript"})
	url := js.Global().Get("URL").Call("createObjectURL", blob)
	module, err := await(js.Global().Call("eval", "(url)=>import(url)").Invoke(url))
	if err != nil {
		log.Fatal(err)
	}
	return module
}

func await(promise js.Value) (js.Value, error) {
	done := make(chan struct{})
	var value js.Value
	var awaitErr error
	resolve := js.FuncOf(func(_ js.Value, args []js.Value) any {
		value = args[0]
		close(done)
		return nil
	})
	reject := js.FuncOf(func(_ js.Value, args []js.Value) any {
		awaitErr = fmt.Errorf("%s", jsErrorString(args[0]))
		close(done)
		return nil
	})
	promise.Call("then", resolve).Call("catch", reject)
	<-done
	resolve.Release()
	reject.Release()
	return value, awaitErr
}

func jsErrorString(value js.Value) string {
	if value.Type() == js.TypeString {
		return value.String()
	}
	if value.Type() == js.TypeUndefined || value.Type() == js.TypeNull {
		return "JavaScript promise rejected without an error"
	}
	return value.Call("toString").String()
}

func newP9Handler() js.Value {
	// Keep the hot request/reply bridge in JavaScript. The previous js.FuncOf
	// implementation entered Go-Wasm for the request, again for the Promise
	// executor, and once more for the reply. A cache=none 9P read/write sends
	// roughly 2,000 four-KiB messages in the comparison workload, making those
	// crossings material even though the actual filesystem server is unchanged.
	// WANIX adapts those messages back into a stream-oriented Go 9P server.
	// Keep that endpoint single-flight: its filesystem/path-lock layer can
	// deadlock when a metadata mutation overlaps another request. The emulator's
	// generic external-9P API remains capable of multiplexing other handlers.
	return js.Global().Call("eval", p9HandlerSource).
		Invoke(js.Global().Get("worker").Get("p9"))
}

func newExportChannel() (js.Value, func(js.Value)) {
	channel := js.Global().Get("MessageChannel").New()
	port1, port2 := channel.Get("port1"), channel.Get("port2")
	buf := make([]byte, 0, 4096)
	signaled := false
	return port2, func(chunk js.Value) {
		bytes := make([]byte, chunk.Get("byteLength").Int())
		js.CopyBytesToGo(bytes, chunk)
		if !signaled && len(bytes) > 0 {
			signaled = true
			port1.Call("postMessage", bytes[0])
			bytes = bytes[1:]
		}
		buf = append(buf, bytes...)
		for len(buf) >= 4 {
			n := int(binary.LittleEndian.Uint32(buf[:4]))
			if n < 4 || len(buf) < n {
				break
			}
			out := js.Global().Get("Uint8Array").New(n)
			js.CopyBytesToJS(out, buf[:n])
			port1.Call("postMessage", out)
			buf = buf[n:]
		}
	}
}

// forwardWinch mirrors the term device's winch signal into the guest: the
// shell's wanix-term publishes "cols rows xpixel ypixel" frames on fit and
// on every resize, and the virtio-console resize path (hvc_resize) turns
// them into tty winsize updates and SIGWINCH for the foreground job. The
// signal file replays the last frame to a new reader, so the first read
// already carries the current size once one has been published.
func forwardWinch(vm js.Value) {
	// The VM task's namespace exposes its term at #task/self/term (the
	// wanix-vm element binds it there); the winch signal file sits under
	// it. Terminal tasks bind a bare "winch" at the root, VM tasks do
	// not, so open the full path.
	winch, err := os.Open("#task/self/term/winch")
	if err != nil {
		log.Println("winch open:", err)
		return
	}
	defer winch.Close()
	buf := make([]byte, 64)
	for {
		n, err := winch.Read(buf)
		if n > 0 {
			fields := strings.Fields(string(buf[:n]))
			if len(fields) >= 2 {
				cols, cerr := strconv.Atoi(fields[0])
				rows, rerr := strconv.Atoi(fields[1])
				if cerr == nil && rerr == nil && cols > 0 && rows > 0 {
					// rv64's resize(cols, rows) writes the standard
					// virtio_console_config order, so no swap is needed
					// (unlike the copy/v86 bus handler).
					vm.Call("resize", cols, rows)
				}
			}
		}
		if err != nil {
			break
		}
	}
}

func copyStdin(vm js.Value) {
	buf := make([]byte, 4096)
	for {
		n, err := os.Stdin.Read(buf)
		if n > 0 {
			out := js.Global().Get("Uint8Array").New(n)
			js.CopyBytesToJS(out, buf[:n])
			vm.Get("console").Call("send", out)
		}
		if err != nil {
			if err != io.EOF {
				log.Println(err)
			}
			return
		}
	}
}
