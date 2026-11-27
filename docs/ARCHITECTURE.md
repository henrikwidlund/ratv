# Architecture

Dependencies between the crates (an arrow means "uses"):

```mermaid
flowchart TD
    ratv["ratv<br/>public API, tokio"]
    companion["ratv-companion"]
    airplay["ratv-airplay"]
    mrp["ratv-mrp"]
    discovery["ratv-discovery"]
    hap["ratv-hap"]
    codec["ratv-codec"]
    ratv --> companion & airplay & mrp & discovery & hap & codec
    companion --> hap & codec
    airplay --> hap & codec
    mrp --> codec
    hap --> codec
```

The `companion`, `discovery` and `mrp` features turn on the optional
crates. `ratv-hap` is always used.

How a client uses them. Pairing runs one time. Keep the `Credentials` for
later connections.

```mermaid
flowchart LR
    scan["discovery::scan<br/>mDNS"] -->|"Device → Endpoints"| pair["pair<br/>HAP pair setup, PIN"]
    pair -->|Credentials| connect["AppleTv::connect<br/>HAP pair verify"]
    connect --> comp["Companion<br/>buttons, apps, power,<br/>volume, text, touch, Siri"]
    connect --> ap["AirPlay 2<br/>RTSP, data stream"]
    ap --> mrp["MRP<br/>now playing, playback,<br/>queue, volume, Siri"]
    comp -->|state, position, artwork| np["AppleTv::now_playing"]
    mrp -->|metadata| np
```
