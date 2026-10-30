return function(reloaded)
    local fill = if reloaded then '#7997ff' else '#28b3a1'
    local radius = ui.motion(0, { kind = 'transition', duration = 200 })
    ui.timer { interval = 1, autostart = true, on_timeout = function() radius:to(40) end }
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
                tile(20, 20, 80, 80, { clip = 'rounded', radius = function() return radius:get() end },
                    { color(80, 80) }),
                tile(140, 20, 80, 80, { radius = 40 }, { color(80, 80) }),
                tile(260, 20, 100, 60, { clip = 'rounded', radius = 1000 }, { color(100, 60) }),
                tile(400, 20, 80, 80, { clip = 'rounded', radius = 40, rotation = 45, scale_x = -1 },
                    { color(80, 80) }),
                tile(20, 140, 100, 100, { clip = 'rounded', radius = 30, background = '#e0a451' }, {
                    tile(20, 20, 80, 80, { clip = 'rounded', radius = 40 }, { color(80, 80) }),
                }),
                tile(160, 140, 80, 80, { clip = 'rounded', radius = 40,
                    box_shadow = { color = '#000000', offset_y = 12 } }, {
                    ui.row { style = { width = 20, height = 20,
                        box_shadow = { color = '#000000', offset_y = 10 } } },
                }),
            },
        } },
    }
end
