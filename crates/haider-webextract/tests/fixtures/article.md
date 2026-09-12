# Rust memory safety

By Ada Lovelace

Rust prevents use after free and data races while keeping predictable performance.

The ownership model makes every reference easy to audit, and the compiler explains how to fix mistakes.

## Example

```
let message = String::from("hello");
println!("{message}");
```

| Rule | Result |
| --- | --- |
| Ownership | One responsible owner |
| Borrowing | Checked references |
