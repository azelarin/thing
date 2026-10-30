return function(reloaded)
    local label = ui.state('left')
    local left_center = ui.state(-100)
    local right_center = ui.state(-100)
    local left_color = if reloaded then '#e0a451' else '#28b3a1'
    local right_color = if reloaded then '#c470d2' else '#7997ff'
    local left = ui.row {
        style = { height = 40, padding_x = 12, radius = 8, background = left_color },
        children = { ui.text {
            text = function() return label:get() end,
            style = { font_family = 'DejaVu Sans', font_size = 14 },
        } },
    }
    local right = ui.row {
        style = { height = 40, padding_x = 12, radius = 8, background = right_color, translate_x = 12 },
        children = { ui.text { text = 'right',
            style = { font_family = 'DejaVu Sans', font_size = 14 } } },
    }
    ui.timer {
        interval = 300, autostart = true,
        on_timeout = function()
            label:set(if reloaded then 'the label grows after reload' else 'a much wider left label')
        end,
    }
    ui.timer {
        interval = 40, repeating = true, autostart = true,
        on_timeout = function()
            local a, b = ui.bounds(left), ui.bounds(right)
            if a and b and a.surface == b.surface then
                left_center:set(a.visual.x + a.visual.width / 2)
                right_center:set(b.visual.x + b.visual.width / 2)
            end
        end,
    }
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 640, height = 200, padding = 20, background = '#0d1117' },
        children = { ui.column {
            style = { width = 600, height = 160, gap = 20, align = 'start' },
            children = {
                ui.row { style = { width = 600, height = 40, gap = 24 }, children = { left, right } },
                ui.stack {
                    style = { width = 600, height = 40, align = 'start', justify = 'start' },
                    children = {
                        ui.row { style = { width = 8, height = 40, background = left_color,
                            translate_x = function() return left_center:get() - 24 end } },
                        ui.row { style = { width = 8, height = 40, background = right_color,
                            translate_x = function() return right_center:get() - 24 end } },
                    },
                },
            },
        } },
    }
end
