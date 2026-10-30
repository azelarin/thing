return function(reloaded)
    local first = if reloaded then '#7997ff' else '#28b3a1'
    local last = if reloaded then '#e0a451' else '#c470d2'
    local function item(color)
        return ui.button {
            style = { width = 100, height = 60, background = color,
                hover_overlay = '#00000000', focus_ring_width = 0 },
        }
    end
    local target = item(last)
    local nested = item(last)
    ui.timer { interval = 100, autostart = true, on_timeout = function()
        ui.reveal(target)
        ui.reveal(nested)
    end }
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 640, height = 240, padding = 20, background = '#0d1117' },
        children = { ui.column {
            style = { width = 600, height = 200, gap = 20, align = 'start' },
            children = {
                ui.scroll { axis = 'horizontal',
                    style = { width = 160, height = 60, radius = 20, clip = 'rounded' },
                    children = { item(first), item('#47d16c'), target },
                },
                ui.scroll { style = { width = 160, height = 60 }, children = {
                    ui.row { style = { width = 160, height = 60, background = '#e84bb5' } },
                    ui.scroll { axis = 'horizontal', style = { width = 160, height = 60 },
                        children = { item(first), item('#47d16c'), nested } },
                } },
            },
        } },
    }
end
