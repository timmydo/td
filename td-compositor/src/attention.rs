use crate::ui;

/// Display-only pixels: ordinary scene rendering never calls this painter.
pub(crate) fn paint(frame: &mut [u8], width: usize, height: usize, stride: usize, draining: bool) {
    let bounds = (0, 0, width, height);
    ui::fill(frame, width, height, stride, bounds, [0x28, 0x20, 0x18, 0]);
    let top = height.saturating_sub(104) / 2;
    for (index, text) in [
        "TD SECURE ATTENTION",
        "NO AUTHORIZATION REQUEST",
        if draining {
            "RELEASE KEYS AND BUTTONS"
        } else {
            "ESC TO RETURN"
        },
    ]
    .into_iter()
    .enumerate()
    {
        ui::draw_text_clipped(
            frame,
            width,
            height,
            stride,
            24,
            top.saturating_add(index.saturating_mul(36)),
            2,
            text,
            [0xff, 0xff, 0xff, 0],
            bounds,
        );
    }
}
