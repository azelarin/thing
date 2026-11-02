return function(progress)
    local function box(x, y, width, height, color, children)
        return ui.stack {
            style = { width = width, height = height, translate_x = x, translate_y = y,
                background = color, align = 'start', justify = 'start' },
            children = children,
        }
    end
    local function pair(color)
        return box(0, 0, 120, 60, '#00000000', {
            box(0, 0, 80, 40, color), box(40, 0, 80, 40, color),
        })
    end
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 560, height = 230, background = '#181818' },
        children = { box(0, 0, 560, 230, '#00000000', {
            ui.crossfade {
                progress = progress,
                style = { width = 160, height = 80, translate_x = 20, translate_y = 20,
                    background = '#00ff00', align = 'start', justify = 'start' },
                children = { pair('#ff0000'), pair('#0000ff') },
            },
            ui.crossfade {
                progress = progress,
                style = { width = 160, height = 80, translate_x = 220, translate_y = 20,
                    align = 'start', justify = 'start' },
                children = {
                    box(0, 0, 60, 40, '#ff000080'), box(30, 0, 60, 40, '#0000ff'),
                },
            },
            ui.crossfade {
                progress = progress,
                enabled = false,
                style = { width = 160, height = 80, translate_x = 20, translate_y = 130,
                    opacity = 0.5, disabled_opacity = 0.5, align = 'start', justify = 'start' },
                children = { pair('#ff0000'), pair('#0000ff') },
            },
            box(420, 130, 100, 60, '#28b3a1'),
        }) },
    }
end
