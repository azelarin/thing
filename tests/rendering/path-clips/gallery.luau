-- path clips: a holed ripple, a filled ripple, a half-turned triangle, nested
-- clips, and shadows around a circular clip. the reload changes the fill and
-- the first ring's fill rule.
return function(reloaded)
    local fill = if reloaded then '#7997ff' else '#28b3a1'
    local ring = 'M40 0 A40 40 0 1 1 40 80 A40 40 0 1 1 40 0 Z M28 28 H52 V52 H28 Z'
    local circle = 'M40 0 A40 40 0 1 1 40 80 A40 40 0 1 1 40 0 Z'
    local function shape(path, rule)
        return { path = path, view_box = { x = 0, y = 0, width = 80, height = 80 },
            fit = 'fill', fill_rule = rule }
    end
    local function tile(x, y, width, height, style, children)
        style.width, style.height = width, height
        style.translate_x, style.translate_y = x, y
        style.align, style.justify = 'start', 'start'
        return ui.stack { style = style, children = children }
    end
    local function color(width, height)
        return ui.row { style = { width = width, height = height, background = fill } }
    end
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 640, height = 320, background = '#0d1117' },
        children = { ui.stack {
            style = { width = 640, height = 320, align = 'start', justify = 'start' },
            children = {
                tile(20, 20, 80, 80, { clip = shape(ring, if reloaded then 'nonzero' else 'evenodd') },
                    { color(80, 80) }),
                tile(140, 20, 80, 80, { clip = shape(ring, 'nonzero') }, { color(80, 80) }),
                tile(260, 20, 80, 80, { clip = shape('M40 0 L80 80 L0 80 Z'), rotation = 180 },
                    { color(80, 80) }),
                tile(20, 140, 100, 100, { clip = shape(circle), background = '#e0a451' }, {
                    tile(20, 20, 80, 80, { clip = shape('M0 0 H40 V80 H0 Z') }, { color(80, 80) }),
                }),
                tile(160, 140, 80, 80, { clip = shape(circle),
                    box_shadow = { color = '#000000', offset_y = 12 } }, {
                    ui.row { style = { width = 80, height = 20,
                        box_shadow = { color = '#000000', offset_y = 50 } } },
                }),
            },
        } },
    }
end
