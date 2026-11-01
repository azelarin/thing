-- a rounded clipping card holds a rotated child that overflows it, so an
-- inspector overlay on the child shows the clipped, transformed bounds. the
-- plain region to the right must look the same with or without an overlay.
local function tile(key, x, y, width, height, style, children)
    style.width, style.height = width, height
    style.translate_x, style.translate_y = x, y
    style.align, style.justify = 'start', 'start'
    return ui.stack { key = key, style = style, children = children }
end

return ui.panel {
    anchors = { top = true, left = true },
    style = { width = 640, height = 240, background = '#0d1117' },
    children = { ui.stack {
        style = { width = 640, height = 240, align = 'start', justify = 'start' },
        children = {
            tile('card', 40, 40, 220, 160, {
                clip = 'rounded',
                radius = 28,
                background = '#28b3a1',
            }, {
                tile('rotated', 30, 50, 260, 60, {
                    rotation = 20,
                    background = '#e0a451',
                }, {}),
            }),
            tile('plain', 380, 40, 220, 160, { background = '#7997ff' }, {}),
        },
    } },
}
