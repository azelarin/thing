-- a: an ellipse on a wide box; the reload makes it a circle on the left.
-- b: white fading to transparent. c: a hard stop between two rings.
-- d: a mirrored, off-center circle. e: an even-odd path with a hole.
return function(reload)
    local function radial(center, radius, stops)
        return { kind = 'radial', center = center, radius = radius, stops = stops }
    end
    local fade = {
        { position = 0, color = '#ffffff' },
        { position = 1, color = '#000000' },
    }
    local ellipse = if reload
        then radial({ x = 0.25, y = 0.5 }, { x = 0.25, y = 0.5 }, fade)
        else radial({ x = 0.5, y = 0.5 }, { x = 0.5, y = 0.5 }, fade)
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 460, height = 240, padding = 10, background = '#202020' },
        children = { ui.column {
            style = { width = 440, height = 220, gap = 20 },
            children = {
                ui.row { style = { width = 440, height = 100, gap = 20 }, children = {
                    ui.row { style = { width = 200, height = 100, background = ellipse } },
                    ui.row { style = { width = 100, height = 100, background = radial(
                        { x = 0.5, y = 0.5 }, { x = 0.5, y = 0.5 }, {
                            { position = 0, color = '#ffffff' },
                            { position = 1, color = '#ffffff00' },
                        }
                    ) } },
                    ui.row { style = { width = 100, height = 100, background = radial(
                        { x = 0.5, y = 0.5 }, { x = 0.5, y = 0.5 }, {
                            { position = 0, color = '#ff0000' },
                            { position = 0.5, color = '#ff0000' },
                            { position = 0.5, color = '#0000ff' },
                            { position = 1, color = '#0000ff' },
                        }
                    ) } },
                } },
                ui.row { style = { width = 440, height = 100, gap = 20 }, children = {
                    ui.row { style = {
                        width = 200,
                        height = 100,
                        scale_x = -1,
                        background = radial({ x = 0.25, y = 0.5 }, { x = 0.25, y = 0.5 }, fade),
                    } },
                    ui.path {
                        data = 'M0 0H100V100H0Z M30 30H70V70H30Z',
                        view_box = { x = 0, y = 0, width = 100, height = 100 },
                        fill_rule = 'evenodd',
                        fill = radial({ x = 0.5, y = 0.5 }, { x = 0.5, y = 0.5 }, fade),
                        style = { width = 100, height = 100 },
                    },
                } },
            },
        } },
    }
end
