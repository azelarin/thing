return function(opacity)
    local function box(width, height, color, x, y)
        return ui.row { style = {
            width = width, height = height, background = color,
            translate_x = x or 0, translate_y = y or 0,
        } }
    end
    local function group(x, y, width, height, shadow, children, extra)
        local style = {
            width = width, height = height, translate_x = x, translate_y = y,
            align = 'start', justify = 'start', opacity = opacity, drop_shadow = shadow,
        }
        for key, value in extra or {} do style[key] = value end
        return ui.stack { style = style, children = children }
    end
    local down = { color = '#000000', offset_y = 40 }
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 580, height = 240, background = '#ffffff' },
        children = { ui.stack {
            style = { width = 580, height = 240, align = 'start', justify = 'start' },
            children = {
                group(20,20,100,30,down,{
                    box(60,30,'#ff0000'), box(60,30,'#ff0000',30),
                }),
                group(180,20,80,80,nil,{
                    group(85,20,40,30,{ color='#000000',offset_x=-40 },{
                        box(40,30,'#ff0000'),
                    }),
                },{ clip='rounded',radius=10,opacity=1 }),
                group(20,120,40,20,down,{
                    ui.image { source=ui.asset('./hole.svg'),style={width=40,height=20} },
                }),
                group(180,120,150,30,{ color='#000000',offset_y=40,blur=2 },{
                    ui.text { text='thing',style={font_size=20,foreground='#000000'} },
                }),
                ui.crossfade {
                    progress=0.5,
                    style={width=100,height=30,translate_x=360,translate_y=20,
                        align='start',justify='start',opacity=opacity,drop_shadow=down},
                    children={box(40,30,'#ff0000'),box(80,30,'#0000ff',20)},
                },
                group(480,20,80,80,nil,{
                    group(70,20,40,30,{color='#000000',offset_x=-20},{
                        box(40,30,'#ff0000'),
                    }),
                },{opacity=1}),
                box(30,30,'#0000ff',520,120),
            },
        } },
    }
end
