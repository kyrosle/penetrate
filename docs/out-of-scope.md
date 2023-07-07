# Out of Scope

`rathole` focuses on the forwarding for the NAT traversal, rather than being a all-in-one development tool or a load balancer or a gateway. It's designed to _be used with them_, not _replace them_.

But that doesn't mean it's not useful for other purposes. In the future, more configuration APIs will be added and `rathole` can be used with an external dashboard.

> Make each program do one thing well.

- _Domain based forwarding for HTTP_

  Introducing these kind of features into `rathole` itself ultimately reinvent a nginx. Use nginx to do this and set `rathole` as the upstream. This method achieves better performance as well as flexibility.

  ```mermaid
  graph LR
    subgraph Nginx
        A[Client]
        B[Nginx]
    end

    subgraph Rathole
        C[Rathole]
    end

    subgraph Upstream Servers
        D[Server 1]
        E[Server 2]
        F[Server 3]
    end

    A -->|HTTP Request| B
    B -->|Forward Request| D
    B -->|Forward Request| E
    B -->|Forward Request| F
    D -->|HTTP Response| B
    E -->|HTTP Response| B
    F -->|HTTP Response| B
    B -->|HTTP Response| A

    B -->|Forward Request| C
    C -->|HTTP Response| B
  ```

- _HTTP Request Logging_

  `rathole` doesn't interference with the application layer traffic. A right place for this kind of stuff is the web server, and a network capture tool.

- _`frp`'s STCP or other setup that requires visitors' side configuration_

  If that kind of setup is possible, then there are a lot more tools available. You may want to consider secure tunnels like wireguard or zerotier. `rathole` primarily focuses on NAT traversal by forwarding, which doesn't require any setup for visitors.

- _Caching `local_ip`'s DNS records_

  As responded in [issue #183](https://github.com/rapiz1/rathole/issues/183), `local_ip` cache is not feasible because we have no reliable way to detect ip change. Handle DNS TTL and so on should be done with a DNS server, not a client. Caching ip is generally dangerous for clients. If you care about the `local_ip` query you can set up a local DNS server and enable caching. Then the local lookup should be trivial.

  - IP change:
    In a network environment, the Internet Protocol Address may change. If the client side caches the old Internet Protocol Address and the service has migrated to the new Internet Protocol Address, the client side will not be able to establish a connection with the service, resulting in a communication outage.

  - DNS cache invalidation:
    If the client side caches DNS records and the DNS server's records are changed or expired, the client side may continue to use the expired cache records instead of querying the latest DNS records. This could result in inaccessibility to the latest service, or it could cause the client side to connect to a malicious fake service.

  - Dynamic load balancing:
    Many services use dynamic load balancing to distribute traffic to multiple servers. If the client side caches the Internet Protocol Address of a specific server and continues to send traffic to that Internet Protocol Address, the benefits of dynamic load balancing cannot be realized, resulting in uneven load and performance issues.

  - Security:
    Caching Internet Protocol Addresses may expose server information, making it easier for attackers to conduct targeted attacks, such as bypassing certain security safeguards by directly attacking known cached Internet Protocol Addresses
