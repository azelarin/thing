-- group opacity cases on a light 580 by 240 panel. every box is opaque
-- #ff0000 and every fade is drawn over the #f0f0f0 background, so the harness
-- can compare each overlap with the single boxes beside it.
return function(options)
    local opacity = options.opacity
    local grouped = options.composite
    local function red(x, y, width, height, shadow)
        return ui.row { style = {
            width = width, height = height, translate_x = x, translate_y = y,
            background = '#ff0000', box_shadow = shadow,
        } }
    end
    -- two 80 by 50 boxes that overlap by 40 pixels.
    local function pair()
        return { red(0, 0, 80, 50), red(40, 0, 80, 50) }
    end
    local function cell(x, y, style, children, properties)
        local merged = {
            width = 120, height = 50, translate_x = x, translate_y = y,
            align = 'start', justify = 'start',
        }
        for key, value in style do
            merged[key] = value
        end
        properties = properties or {}
        properties.style = merged
        properties.children = children
        return ui.stack(properties)
    end
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 580, height = 240, background = '#f0f0f0' },
        children = { ui.stack {
            style = { width = 580, height = 240, align = 'start', justify = 'start' },
            children = {
                -- a: without compositing, each box fades alone and the
                -- overlap compounds.
                cell(20, 20, { opacity = opacity }, pair()),
                -- b: the same boxes fade once as a group.
                cell(160, 20, { opacity = opacity, composite = grouped }, pair()),
                -- c: a half-opaque plain ancestor multiplies the group once.
                cell(300, 20, { opacity = 0.5 }, {
                    cell(0, 0, { opacity = opacity, composite = grouped }, pair()),
                }),
                -- d: a disabled ancestor dims the group once, and the group's
                -- own disabled opacity does not compound with it.
                cell(440, 20, { disabled_opacity = 0.5 }, {
                    cell(0, 0, { opacity = opacity, composite = grouped, disabled_opacity = 0.1 },
                        pair(), { enabled = false }),
                }, { enabled = false }),
                -- e: a rounded ancestor clips a larger group, the boxes that
                -- overflow it, and a hard shadow below the second box.
                cell(20, 100, { width = 200, height = 105, clip = 'rounded', radius = 40 }, {
                    cell(0, 0, { width = 240, height = 140, opacity = opacity, composite = grouped }, {
                        red(0, 0, 130, 50),
                        red(70, 10, 160, 50, { color = '#000000', offset_y = 70 }),
                    }),
                }),
                -- f: an opaque sibling painted after the clipped group.
                ui.row { style = {
                    width = 100, height = 100, translate_x = 260, translate_y = 100,
                    background = '#0000ff',
                } },
            },
        } },
    }
end
