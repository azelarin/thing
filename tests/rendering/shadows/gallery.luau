-- shadow cases on a light 600 by 440 panel. each shape sits 30 pixels inside a
-- 120 pixel cell, so the harness can sample well away from antialiased edges.
return function(options)
    local function hard(x, y, spread)
        return { color = '#000000', offset_x = x, offset_y = y, spread = spread }
    end
    local function merged(base, extra)
        for key, value in extra or {} do
            base[key] = value
        end
        return base
    end
    local function box(shadow, style)
        return ui.row {
            style = merged({ width = 60, height = 60, background = '#ffffff', box_shadow = shadow }, style),
        }
    end
    local function path(data, shadow, rule)
        return ui.path {
            data = data,
            view_box = { x = 0, y = 0, width = 60, height = 60 },
            fill = '#00a000',
            fill_rule = rule or 'nonzero',
            shadow = shadow,
            style = { width = 60, height = 60 },
        }
    end
    local function cell(child, style)
        return ui.row {
            style = merged(
                { width = 120, height = 120, padding = 30, align = 'start', justify = 'start' },
                style
            ),
            children = { child },
        }
    end
    local function line(children)
        return ui.row {
            style = { width = 540, height = 120, gap = 20, align = 'start' },
            children = children,
        }
    end
    return ui.panel {
        anchors = { top = true, left = true },
        style = {
            width = 600,
            height = 440,
            padding = 20,
            background = '#e0e0e0',
            align = 'start',
            justify = 'start',
        },
        children = { ui.column {
            style = { width = 540, height = 400, gap = 20, align = 'start' },
            children = {
                line {
                    -- a transparent rounded box keeps its center clear.
                    cell(ui.row { style = { width = 60, height = 60, radius = 12, box_shadow = hard(10, 10, 0) } }),
                    cell(box({ color = '#000000', blur = 16, offset_y = 12 })),
                    -- an ordinary container lets the shadow escape; a viewport clips it.
                    cell(ui.column { style = { width = 60, height = 60 }, children = { box(hard(0, 20, 0)) } }),
                    cell(ui.scroll { style = { width = 60, height = 60 }, children = { box(hard(0, 20, 0)) } }),
                },
                line {
                    cell(path('M0 0H60V20H20V60H0Z', hard(8, 8, 0))),
                    cell(path('M0 0H60V60H0Z M15 15H45V45H15Z', hard(0, 0, 6), 'evenodd')),
                    cell(path('M0 0H60V60H0Z', hard(0, 24, -10))),
                    cell(box(hard(12, 0, 0), { scale_x = -1 })),
                },
                line {
                    cell(box(hard(0, 20, 0)), { opacity = 0.5 }),
                    cell(box(hard(0, 20, 0), { rotation = 90 })),
                    cell(box(options.shadow)),
                },
            },
        } },
    }
end
