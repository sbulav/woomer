#version 330

// Input vertex attributes (from vertex shader)
in vec2 fragTexCoord;
in vec4 fragColor;

// Input uniform values
uniform sampler2D texture0;
uniform vec4 colDiffuse;

uniform vec4 spotlightTint;
uniform vec2 cursorPosition;
uniform float spotlightRadiusSquared;
uniform vec2 textureSize;

// Output fragment color
out vec4 finalColor;

void main()
{
    // Texel color fetching from texture sampler
    vec4 texelColor = texture(texture0, fragTexCoord);

    // Compare in screenshot coordinates instead of framebuffer coordinates.
    vec2 imagePosition = fragTexCoord * textureSize;
    vec2 cursorDelta = imagePosition - cursorPosition;

    if (dot(cursorDelta, cursorDelta) > spotlightRadiusSquared)  {
        finalColor = (mix(texelColor, vec4(spotlightTint.rgb, 1.0), spotlightTint.a) * colDiffuse);
    } else {
        finalColor = (texelColor * colDiffuse);
    }
}
