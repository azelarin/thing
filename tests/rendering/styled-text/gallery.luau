return function(reloaded)
    local accent = if reloaded then '#47d16c' else '#ff3030'
    local function place(x, y, width, height, widget)
        return ui.stack {
            style = {
                width = width, height = height, translate_x = x, translate_y = y,
                align = 'start', justify = 'start',
            },
            children = { widget },
        }
    end
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 640, height = 200, background = '#0d1117' },
        children = { ui.stack {
            style = { width = 640, height = 200, align = 'start', justify = 'start' },
            children = {
                -- a colored bold span beside text in the widget's foreground.
                place(20, 10, 600, 60, ui.text {
                    spans = { { text = 'MMMM', color = accent, bold = true }, ' MMMM' },
                    style = { width = 600, font_size = 40, foreground = '#7997ff' },
                }),
                -- an underlined link with a letter-spaced span.
                place(20, 80, 600, 40, ui.text {
                    spans = { { text = 'linked text', link = 'https://example.org', color = '#e0a451' } },
                    on_link = function() end,
                    style = { width = 600, font_size = 24, letter_spacing = 1, foreground = '#7997ff' },
                }),
                -- two lines, then an ellipsis; nothing paints on a third line.
                place(20, 120, 200, 70, ui.stack {
                    style = { width = 200, height = 70, background = '#28b3a1', align = 'start', justify = 'start' },
                    children = { ui.text {
                        text = 'wrapping words that continue well past two lines of this narrow box',
                        max_lines = 2,
                        overflow = 'ellipsis',
                        style = { width = 200, font_size = 14, foreground = '#ffffff' },
                    } },
                }),
            },
        } },
    }
end
