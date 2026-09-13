# Support Matrix — Provider Status

> Known fixtures for provider support status across Runtimo runtime.

## VERIFIED Providers

| Provider | Type | Status | Version | Privileges |
|----------|------|--------|---------|------------|
| Tetragon | Kernel tracing | VERIFIED | 1.7.1 | CAP_SYS_ADMIN, CAP_DAC_READ_SEARCH |
| JFR | JVM profiling | VERIFIED | JDK 17 | CAP_SYS_ADMIN (optional) |

## DEGRADED Providers

| Provider | Type | Status | Version | Privileges |
|----------|------|--------|---------|------------|
| Tetragon | Kernel tracing | DEGRADED | <0.9 | Limited kernel access |
| JFR | JVM profiling | DEGRADED | <11 | Requires JMX |

## DEFERRED Providers

| Provider | Type | Status | Version | Privileges |
|----------|------|--------|---------|------------|
| OTel | Observability | DEFERRED | 0.12+ | None (library-level) |
| OBI | Object binary | DEFERRED | — | None |
| Symbolic | Symbol resolution | DEFERRED | — | None |
| EventPipe | .NET tracing | DEFERRED | .NET 6+ | None |

## UNAVAILABLE Providers

| Provider | Type | Status | Version | Privileges |
|----------|------|--------|---------|------------|
| OTel | Observability | UNAVAILABLE | — | — |
| OBI | Object binary | UNAVAILABLE | — | — |
| Symbolic | Symbol resolution | UNAVAILABLE | — | — |
| EventPipe | .NET tracing | UNAVAILABLE | — | — |

## UNTESTED Providers

| Provider | Type | Status | Version | Privileges |
|----------|------|--------|---------|------------|
| OTel | Observability | UNTESTED | — | — |
| OBI | Object binary | UNTESTED | — | — |
| Symbolic | Symbol resolution | UNTESTED | — | — |
| EventPipe | .NET tracing | UNTESTED | — | — |

## Notes

- VERIFIED: Full integration tested, roundtrip confirmed
- DEGRADED: Partial functionality, known limitations
- DEFERRED: Planned but not yet implemented
- UNAVAILABLE: Not available in current environment
- UNTESTED: Not yet tested
