# Every construct

## Inline marks

Plain, **bold**, *italic*, ~~struck~~, `code`, and ***both***.

A [titled link](https://example.com/a "Example A") with [**bold link text**](https://example.com/b) and a `` code `tick` `` span.

Marks across a *run with **nested bold** inside* italic.

Literal characters: 2 \* 3, snake_case_name, a_b\_ edge, \<div> and a [bracket\](not-a-link).

Line one with a hard break\
line two.

### Lists

1. one
2. two
   1. two-a
   2. two-b
      - deep bullet
3. three

- [ ] open
  - [x] nested done
- [x] done

> Quote paragraph one
> continues here.
>
> Quote paragraph two with **bold**.

```rust
fn main() {
    println!("hi");
}
```

````md
```nested fence```
````

| Left | Center | Right | Plain |
| :--- | :---: | ---: | --- |
| **b** | *i* | `c` | [l](https://x.test) |
| pipe \| inside | ![cell image](https://x.test/c.png) | 3 | |

![An image](https://example.com/img.png "Image title")

Text with a footnote[^note] reference.

---

Closing paragraph.

[^note]: The footnote body.
