-- six 120 by 80 cells over a dark background. each row of the pattern is red,
-- green, and half-transparent blue from left to right.
local pattern = ui.asset('./pattern.svg')

local function cell(properties)
    properties.source = pattern
    properties.style = properties.style or {}
    properties.style.width = 120
    properties.style.height = 80
    return ui.image(properties)
end

return function(options)
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 410, height = 190, padding = 10, background = '#181818' },
        children = { ui.column {
            style = { width = 390, height = 170, gap = 10 },
            children = {
                ui.row { style = { width = 390, height = 80, gap = 15 }, children = {
                    -- no effect: sharp band edges, letterboxed by contain.
                    cell { fit = 'contain' },
                    -- blurred cover cropped by a rounded corner.
                    cell {
                        fit = 'cover',
                        blur = options.blur,
                        style = { radius = 16 },
                    },
                    -- grayscale fill; the blue band keeps half transparency.
                    cell { fit = 'fill', saturation = options.saturation },
                } },
                ui.row { style = { width = 390, height = 80, gap = 15 }, children = {
                    -- increased saturation inside padding.
                    cell { fit = 'fill', saturation = 2, style = { padding = 8 } },
                    -- a rotated, half-transparent blur at fractional positions.
                    cell {
                        fit = 'fill',
                        blur = options.blur,
                        style = { opacity = 0.5, rotation = 10, translate_x = 0.5 },
                    },
                    -- an ancestor clips the right half of a blurred image.
                    ui.stack {
                        style = { width = 60, height = 80, align = 'start', justify = 'start' },
                        children = { cell {
                            fit = 'fill',
                            blur = options.blur,
                            style = { shrink = 0 },
                        } },
                    },
                } },
            },
        } },
    }
end
