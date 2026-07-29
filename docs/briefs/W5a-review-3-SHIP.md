# W5a.2 final confirm — SHIP_WITH_FIXES → SHIP (SSRF boundary airtight)

Reviewer: gpt-5.6 (codex), frozen 63504d7. The credential-exfil invariant HOLDS: P0/P1 none. Resolve-validate-pin PASS (every resolved A/AAAA checked incl. IPv6 fe80::/10, fc00::/7, IPv4-mapped, multicast, unspecified; any-forbidden→reject-all; the probe AND turn use the same pinned+no-proxy client; the rebind pin is sound — reqwest consumes the cached resolver once, verified with a real client.execute). Proxy-off PASS on all three key-bearing clients (compatible, native OpenAI, Anthropic). Native hosts fixed/trusted; only the compatible path takes a user base_url. Mutation audit killed both (resolved-address validation, .no_proxy()). 913→918, daemond live 30/30.

Sole finding: P2 functional (not SSRF) — bracketed IPv6 literals (`[::1]`, public IPv6 literals) misclassified as hostnames (host_str brackets vs raw parse), fail-closed. Closed by the orchestrator as W5a.3 (bracket-strip before the literal check + a mutation-checked pin; 918→919).

The W5a SSRF/transport boundary took three fix rounds (IP-literal → hostname-resolve → proxy-inheritance), each closing a real credential-exfil vector; the invariant "no key-bearing request leaves toward a private/link-local/metadata/ULA/non-loopback-plain-HTTP origin or via an env proxy, under any host/IP encoding" now holds.

VERDICT: SHIP (with the P2 folded as W5a.3)
