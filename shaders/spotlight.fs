#version 330

// Input vertex attributes (from vertex shader)
in vec2 fragTexCoord;
in vec4 fragColor;

// Input uniform values
uniform sampler2D texture0;
uniform vec4 colDiffuse;

uniform vec4 spotlightTint;
uniform vec2 cursorPosition;
uniform float spotlightRadiusMultiplier;
uniform float cameraZoom;
uniform vec2 textureSize;

// This is multiplied by spotlightRadiusMultiplier to obtain the radius in pixels
const int UNIT_RADIUS = 60;

// Output fragment color
out vec4 finalColor;

void main()
{
    // Texel color fetching from texture sampler
    vec4 texelColor = texture(texture0, fragTexCoord);

    // Compare in screenshot coordinates instead of framebuffer coordinates.
    vec2 imagePosition = fragTexCoord * textureSize;
    float distanceToCursor = distance(imagePosition, cursorPosition);

    // Keep the visible spotlight radius stable while zooming.
    float spotlightRadius = float(UNIT_RADIUS) * spotlightRadiusMultiplier / cameraZoom;
    if (distanceToCursor > spotlightRadius)  {
        finalColor = (mix(texelColor, vec4(spotlightTint.rgb, 1.0), spotlightTint.a) * colDiffuse);
    } else {
        finalColor = (texelColor * colDiffuse);
    }
}
