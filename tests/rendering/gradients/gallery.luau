return function(reverse)
    local function brush(from, to, stops)
        return { kind = 'linear', from = from, to = to, stops = stops }
    end
    local horizontal = brush({ x = 0, y = 0 }, { x = 1, y = 0 }, {
        { position = 0, color = if reverse then '#ffffff' else '#000000' },
        { position = 1, color = if reverse then '#000000' else '#ffffff' },
    })
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 480, height = 290, padding = 10, background = '#202020' },
        children = { ui.column {
            style = { width = 460, height = 270, gap = 10 },
            children = {
                ui.row { style = { width = 460, height = 50, radius = 10, background = horizontal } },
                ui.row { style = { width = 460, height = 50, background = brush(
                    { x = -100000000, y = 0 }, { x = 100000001, y = 0 }, {
                        { position = 0, color = '#ff0000' },
                        { position = 0.5, color = '#ff0000' },
                        { position = 0.5, color = '#00ff00' },
                        { position = 0.5, color = '#0000ff' },
                        { position = 1, color = '#0000ff' },
                    }
                ) } },
                ui.path {
                    data = 'M0 0H100V10H0Z',
                    view_box = { x = 0, y = 0, width = 100, height = 10 },
                    fit = 'contain',
                    fill = brush({ x = 0, y = 0 }, { x = 0, y = 1 }, {
                        { position = 0, color = '#ff0000' },
                        { position = 1, color = '#0000ff00' },
                    }),
                    style = { width = 460, height = 80, padding = 10, radius = 12 },
                },
                ui.row { style = { width = 460, height = 50, scale_x = -1, background = brush(
                    { x = 0, y = 0 }, { x = 1, y = 0 }, {
                        { position = 0, color = '#000000' },
                        { position = 1, color = '#ffffff' },
                    }
                ) } },
            },
        } },
    }
end
