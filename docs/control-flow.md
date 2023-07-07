# Application control flow

## entry startup

```mermaid
graph TB
  EntryPoint --> |args, shutdown_rx| App
  App --> |"args, \nshutdown_rx(subscribe(shutdown_tx)), \nservice_update"|run_instance
  App --> |config_path,shutdown_rx| cfg_watcher
  App --- shutdown_tx([shutdown_tx])

  run_instance --> |config, args|determine_run_mode
  determine_run_mode --> run_server
  determine_run_mode --> run_client

  cfg_watcher --- event_rx
  I([last_instance])
  J === run_instance
  I --- J([JoinHandle])
  subgraph ConfigChange channel
    R(["Receiver@ConfigChange"])
    S(["Sender@ConfigChange"])
  end
  I --- R
  R --> |service_update| run_instance
  I --- S
  event_rx --> |ConfigChange::General| I
  event_rx --> |ConfigChange::Client/Server \n events| S
```

## config watcher handler

```mermaid
graph TB
  config_path([config_path]) --- config_watcher
  shutdown_rx([shutdown_rx]) --- config_watcher

  config_watcher -.- |Config::from_file| origin_cfg
  subgraph ConfigChange channel
    event_tx
    event_rx
  end
  config_watcher -.- event_tx
  config_watcher -.- event_rx
  subgraph initial state
  event_tx --> |ConfigChange::General| event_rx
  end
  subgraph config watcher
  subgraph notify channel
  T([fevent_tx])
  R([fevent_rx])
  end

  notify::recommended_watcher --> T
  R --> |calculate_events| events
  events --> event_rx
  end
```

## client startup

```mermaid
graph TB
  run_instance --> |config, \nshutdown_rx, \nservice_update| C(run_client)
  C --> |Transport| Tcp
  C --> |Transport| Tls
  C --> |Transport| Noise
```

### client running flow

```mermaid
graph TB
  C(Client::run)
  CC(ControlChannel)
  CCH(ControlChannelHandle)
  config.service --> |iterated| config.service
  C --> config.service --> |"service_config\nremote_addr\ntransport\nheartbeat_timeout"| CCH
  CCH --- backoff([backoff])
  CCH --- sr([shutdown_rx])
  CCH === st([shutdown_tx])
  CCH --> CC
  sr --> CC
  backoff --- |control backoff retry| CC
  subgraph ControlChannel Thread
  CC --> run([run])
  addr(remote_addr)
  run --> |resolve| addr
  addr --> conn
  run --> |connect| conn
  conn --> |hint SocketOptions::for_control_channel| conn
  conn --> |negotiate| next
  end
```

#### negotiate operation

```mermaid
sequenceDiagram
  participant c as client
  participant s as server
  c ->> s: Hello::ControlChannelHello
  s -->> c: nonce
  c ->> s: send Auth(service_token+nonce)
  alt checkAuth ok
    s -->> c: Ack::Ok
    Note left of c: Channel ready
  else checkAuth failed
    s -->> c: Ack::AuthFailed
  end
```

#### startup control channel

```mermaid
graph TB
  conn
  s[[tokio::select]]
  conn --> r[[read_control_cmd]]
  s --> r
  s --> |heartbeat_timeout| t[["time::sleep()"]] --> Error
  s --> shutdown_rx --> break

  r --> c(ControlChannelCmd::CreateDataChannel)
  r --> ControlChannelCmd::HeartBeat --> NoneOperation
  c --> rc[[run_data_channel]]
```

- `read_control_cmd`: read exact `PACKET_LEN.c_cmd` from `conn`, and deserialize it as `ControlChannelCmd`.

#### startup data control channel

`run_data_channel` :

- data channel arguments

```rust
let socket_opts =
  /* use the config marked: nodelay */
  SocketOpts::from_client_cfg(&self.service);

let data_ch_args = Arc::new(RunDataChannelArgs {
  session_key,
  remote_addr,
  connector: self.transport.clone(),
  socket_opts,
  service: self.service.clone(),
});
```

- before data transport: `data_channel_handshake`

```mermaid
sequenceDiagram
  participant c as client
  participant s as server
  note left of c: connector.connect(remote_addr) => conn
  note left of c: hint(conn, socket_opts)
  c ->> s: Hello::DataChannelHello(session_key)
```

- after data channel handshake :

```mermaid
graph TB
  rd[[read_data_cmd]]
  rd --> DataChannelCmd::StartForwardTcp --> |service.local_addr| run_data_channel_for_tcp
  rd --> DataChannelCmd::StartForwardUdp --> |service.local_addr| run_data_channel_for_udp
```

- `run_data_chan  nel_for_tcp`
  main code:

  ```rust
  let mut local =
    TcpStream::connect(local_addr)
  // Copies data in both directions between `conn` and `local`
  tokio::io::copy_bidirectional(&mut conn, &mut local)
  ```

- `run_data_channel_for_udp`

  - udp packet: `UdpTraffic`
    ```rust
    struct UdpHeader {
      /// Sender socket address
      from: SocketAddr,
      /// length of udp packet
      len: UdpPacketLen,
    }
    // packet sequence
    |udp header len| udp header | udp packet data |
    |      u8      |    ...     |       ...       |
    ```
  - `port_map: HashMap<SocketAddr, mpsc::Sender<Bytes>`
  - `outbound_tx` + `outbound_rx` channel with `UDP_BUFFER_SIZE` capacity.
  - `rd: ReadHalf` + `wr: WriteHalf` by `tokio::io::split(conn)`

  ```mermaid
  graph TB
  subgraph channel stores UdpTraffic
    ot(outbound_tx)
    or(outbound_rx)
  end
  subgraph sending items form the outbound channel to the server
    or --> t(UdpTraffic)
    t --> |write| wr --> server((server))
  end

  subgraph read a packet from the server
  rd((rd)) --> read_header --> read_packet --- packet.form --> |check exist| port_map
  end
  port_map --> |is_none| udp_connect
  udp_connect --> |Ok| UdpSocket
  subgraph udp send queue channel
    it(inbound_tx)
    ir(inbound_rx)
  end
  UdpSocket --- it --> |insert| port_map
  UdpSocket --- ir
  ot --> ruf[[run_udp_forward]]
  ir --> ruf
  udp_connect --> Err
  port_map --> |is_some| tx --> |send| packet.data
  ```

  `udp_connect`:
  main code:

  ```rust
  let addr = to_socket_addr(addr).await?;

  let bind_addr = match addr {
    SocketAddr::V4(_) => "0.0.0.0:0",
    SocketAddr::V6(_) => ":::0",
  };

  let s = UdpSocket::bind(bind_addr).await?;
  s.connect(addr).await?;
  ```

  `run_udp_forwarder`

  ```mermaid
  graph TB
    t[[tokio::select]]
    t --> |recv| ir(inbound_rx) --> |send data| s
    t --> |recv| s(UdpSocket)
    s --> |"send (len + UdpTraffic)"| ot(outbound_tx)
    t --> ts[["time::sleep"]]
  ```

## server configure

```mermaid
graph TB
  run_instance --> |config, \nshutdown_rx, \nservice_update| run_server
  run_server --> Tcp
  run_server --> Tls
  run_server --> Noise
```

### server run

```mermaid
graph TB
  cc("control_channels: \n MultiMap@ServiceDigest, Nonce, ControlChannelHandle")
  prev("transport_bind(bind_addr)") --> ac
  ac(acceptor) --> |"accept(acceptor)"| s[[tokio::select]]
  s --> i(io::Error) --> sl[["time::sleep(backoff.next_duration)"]]
  s --> ca((conn, addr))
  ca --> th[[transport.handshake]]
  subgraph handle connection
  th --> |conn| hc[[handle_connection]]
  cc --> hc
  end
```

- `handle_connection`

```mermaid
sequenceDiagram
  participant s as server
  participant c as client
  c ->> s: Hello
  alt Hello::ControlChannelHello
  Note left of s: do_control_channel_handshake `service_digest`
  else Hello::DataChannelHello
  Note left of s: do_data_channel_handshake `nonce`
  end
```

- `do_control_channel_handshake`

```mermaid
sequenceDiagram
  participant s as server
  participant c as client
  Note left of s: hint(conn, SocketOpts::for_control_channel)
  Note left of s: rand a `nonce`
  s ->> c: Hello::ControlChannelHello `nonce`
  c ->> s: Auth
  alt session_key(service_name+nonce)  == auth
    s ->> c: Ack::Ok
    Note left of s: ControlChannelHandle::new
  else
    s ->> c: Auth::AuthFailed
  end
```

### control channel handle

```mermaid
graph LR
  subgraph shutdown broadcast channel
    st(shutdown_tx)
    sr(shutdown_rx)
  end
  subgraph store data channels
    dct(data_ch_tx)
    dcr(data_ch_rx)
  end
  subgraph store data channel creation requests
    dcrt(data_ch_req_tx)
    dcrr(data_ch_req_rx)
  end

  service_type --> Tcp --> rtcp[[run_tcp_connection_pool]] --> tlas[[tcp_listen_and_send]]
  dcrt --> tlas
  dcr --> rtcp
  dcrt --> rtcp
  sr --> rtcp
  service_type --> Udp --> rucp[[run_udp_connection_pool]]
  dcr --> rucp
  dcrt --> rucp
  sr --> rucp

  sr --> cc
  dcrr --> cc
  cc[[channel.run]]

  cch((ControlChannelHandle))
  dct --> cch
  st --> cch
```

- `tcp_listen_and_send -> rx`

```mermaid
graph TB
  subgraph TcpStream handler channel
  tx
  rx
  end
  tlb[["TcpListener::bind(address)"]]
  tlb --> s[[tokio::select]]
  s --> |accept| val
  val --> |Err| ts[["time::sleep(backoff.next_duration)"]]
  val --> ia((incoming, addr)) --> |send true| data_ch_req_tx
  ia --> |send incoming| tx
  s --> |recv| shutdown_rx --> break
```

- `run_tcp_connection_pool`

```mermaid
graph TB
  visitor_rx
  visitor_rx --> |recv| v(visitor)
  data_ch_rx --> |recv| i(incoming)
  i --> |write_all| DataChannelCmd::StartForwardTcp
    i --- cb
    v --- cb
  subgraph write_all is_ok
    cb[[copy_bidirectional]]
  end
  subgraph write_all is_err
    data_ch_req_tx --> |send| true
  end
```

- `run_udp_connection_pool`

```mermaid
graph TB
  ub[["UdpSocket::bin(bind_addr)"]] --> u
  u(UdpSocket) --> |2.DataChannelCmd::StartForwardUdp| conn
  data_ch_rx --> |1.recv| conn
  subgraph tokio::select
    u --> |recv| buf
    buf --> |write_slice| conn

    conn --> read_u8 --> u

    shutdown_rx --> |recv| break
  end
```

### control channel

```mermaid
graph TB
  subgraph tokio::select
    data_ch_req_rx --> |recv| val --> |conn send| ControlChannelCmd::CreateDataChannel
    ts[["time::sleep(heartbeat_interval)"]] --> |conn send| ControlChannelCmd::HeartBeat
  end
```

### data channel handshake

```mermaid
graph LR
  control_channel --> handle
  conn --- h[["hint(conn, SocketOpts::from_server_cfg)"]]
  handle --- data_ch_tx --> |send| conn
  h --> conn
```
